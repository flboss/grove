use std::borrow::Cow;
use std::rc::Rc;

use grove_exec::value::{Scalar, Val};
use grove_query::typecheck::types::{QueryType, RecordSource};
use grove_schema::ValidatedSchema;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::args::{Format, OutputArgs};
use crate::error::{CliError, CliResult};

const RESET: &str = "\x1b[0m";
const STYLE_BOLD: &str = "1";
const STYLE_DIM: &str = "2";
const STYLE_NUMBER: &str = "36";
const STYLE_STRING: &str = "32";
const STYLE_BOOL: &str = "33";
const STYLE_ENUM: &str = "34";
const STYLE_TIME: &str = "35";

pub struct RenderOptions {
    pub format: Format,
    pub color: bool,
    pub width: usize,
    pub max_rows: usize,
    pub max_depth: usize,
    pub max_cell_width: usize,
    pub headers: bool,
}

impl RenderOptions {
    pub fn from_args(args: &OutputArgs, color: bool) -> Self {
        let width = args
            .width
            .or_else(|| {
                std::env::var("COLUMNS")
                    .ok()
                    .and_then(|value| value.parse().ok())
            })
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
        }
    }
}

pub fn render(
    value: &Val,
    ty: Option<&QueryType>,
    schema: &ValidatedSchema,
    options: &RenderOptions,
) -> CliResult<String> {
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
            if items.len() <= limit && within_row_cap && fits_width(ctx, &compact.plain) {
                Ok(compact.styled)
            } else {
                Ok(block_lines(ctx, value, 0).join("\n"))
            }
        }
        Val::Record(_) => Ok(block_lines(ctx, value, 0).join("\n")),
        Val::Tuple(_) => {
            let compact = compact(ctx, value, 0);
            if fits_width(ctx, &compact.plain) {
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

#[derive(Clone)]
struct Styled {
    plain: String,
    styled: String,
}

impl Styled {
    fn painted(text: impl Into<String>, style: &str, color: bool) -> Self {
        let plain = text.into();
        let styled = paint(&plain, Some(style), color);
        Styled { plain, styled }
    }
}

fn compact(ctx: &RenderCtx<'_>, value: &Val, depth: usize) -> Styled {
    if ctx.options.max_depth > 0
        && depth >= ctx.options.max_depth
        && matches!(value, Val::Record(_) | Val::List(_) | Val::Tuple(_))
    {
        return Styled::painted("…", STYLE_DIM, ctx.options.color);
    }

    match value {
        Val::None => Styled::painted("none", STYLE_DIM, ctx.options.color),
        Val::Some(inner) => {
            let inner = compact(ctx, inner, depth);
            Styled {
                plain: format!("some({})", inner.plain),
                styled: format!("some({})", inner.styled),
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
            Styled::painted(text, STYLE_ENUM, ctx.options.color)
        }
        Val::List(items) => {
            let mut plain = String::from("[");
            let mut styled = String::from("[");
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    plain.push_str(", ");
                    styled.push_str(", ");
                }
                let item = compact(ctx, item, depth + 1);
                plain.push_str(&item.plain);
                styled.push_str(&item.styled);
            }
            plain.push(']');
            styled.push(']');
            Styled { plain, styled }
        }
        Val::Tuple(items) => {
            let mut plain = String::from("(");
            let mut styled = String::from("(");
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    plain.push_str(", ");
                    styled.push_str(", ");
                }
                let item = compact(ctx, item, depth + 1);
                plain.push_str(&item.plain);
                styled.push_str(&item.styled);
            }
            plain.push(')');
            styled.push(')');
            Styled { plain, styled }
        }
        Val::Record(fields) => {
            let mut plain = String::from("{");
            let mut styled = String::from("{");
            for (index, (name, value)) in fields.iter().enumerate() {
                if index > 0 {
                    plain.push_str(", ");
                    styled.push_str(", ");
                }
                plain.push_str(name);
                styled.push_str(&paint(name, Some(STYLE_BOLD), ctx.options.color));
                plain.push_str(": ");
                styled.push_str(": ");
                let value = compact(ctx, value, depth + 1);
                plain.push_str(&value.plain);
                styled.push_str(&value.styled);
            }
            plain.push('}');
            styled.push('}');
            Styled { plain, styled }
        }
        Val::Ref { .. } => {
            let text = value.formatter(ctx.schema).to_string();
            Styled::painted(text, STYLE_DIM, ctx.options.color)
        }
    }
}

fn block_lines(ctx: &RenderCtx<'_>, value: &Val, depth: usize) -> Vec<String> {
    if can_expand(ctx, depth) {
        match value {
            Val::Record(fields) => return record_lines(ctx, fields, depth),
            Val::Tuple(items) => return tuple_lines(ctx, items, depth),
            Val::List(items) => return list_lines(ctx, items, depth),
            _ => {}
        }
    }
    vec![inline_cell(ctx, value, depth).styled]
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

fn named_lines(ctx: &RenderCtx<'_>, entries: &[(String, &Val)], depth: usize) -> Vec<String> {
    let key_width = entries
        .iter()
        .map(|(key, _)| display_width(key))
        .max()
        .unwrap_or(0);
    let mut lines = Vec::new();

    for (key, value) in entries {
        let key = pad(key, key_width, Align::Left);
        let mut key = paint(&key, Some(STYLE_BOLD), ctx.options.color);
        if expandable(value) && can_expand(ctx, depth + 1) {
            key.push(':');
            lines.push(key);
            for mut line in block_lines(ctx, value, depth + 1) {
                line.insert_str(0, "  ");
                lines.push(line);
            }
        } else {
            key.push_str(": ");
            key.push_str(&inline_cell(ctx, value, depth + 1).styled);
            lines.push(key);
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

    for item in items {
        if let Val::List(_) = item {
            lines.push("-".to_string());
            for mut line in block_lines(ctx, item, depth + 1) {
                line.insert_str(0, "  ");
                lines.push(line);
            }
        } else if expandable(item) {
            for (index, mut line) in block_lines(ctx, item, depth + 1).into_iter().enumerate() {
                if index == 0 {
                    line.insert_str(0, "- ");
                } else {
                    line.insert_str(0, "  ");
                }
                lines.push(line);
            }
        } else {
            lines.push(format!("- {}", inline_cell(ctx, item, depth + 1).styled));
        }
    }

    if let Some(note) = note {
        lines.push(paint(&note, Some(STYLE_DIM), ctx.options.color));
    }

    lines
}

fn inline_cell(ctx: &RenderCtx<'_>, value: &Val, depth: usize) -> Styled {
    let text = cell_text(ctx, value, depth);
    Styled {
        styled: paint(&text, value_style(value), ctx.options.color),
        plain: text,
    }
}

fn cell_text(ctx: &RenderCtx<'_>, value: &Val, depth: usize) -> String {
    let plain = compact(ctx, value, depth).plain;
    let cap = ctx.options.max_cell_width;
    if cap == 0 || display_width(&plain) <= cap {
        return plain;
    }

    match value {
        Val::List(items) if !items.is_empty() => {
            truncate_display(format!("[{} items]", items.len()), cap)
        }
        Val::Record(fields) if !fields.is_empty() => truncate_display("{…}", cap),
        _ => truncate_display(plain, cap),
    }
}

fn expandable(value: &Val) -> bool {
    match value {
        Val::Record(fields) => !fields.is_empty(),
        Val::Tuple(items) | Val::List(items) => !items.is_empty(),
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
    text: String,
    style: Option<&'static str>,
    align: Align,
}

impl TableCell {
    fn empty() -> Self {
        TableCell {
            text: String::new(),
            style: None,
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
        .map(|header| truncate_display(header, ctx.options.max_cell_width))
        .collect();
    let mut widths: Vec<usize> = headers.iter().map(|header| display_width(header)).collect();
    for row in &rows {
        for (index, cell) in row.iter().enumerate() {
            if let Some(width) = widths.get_mut(index) {
                *width = (*width).max(display_width(&cell.text));
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
        text: cell_text(ctx, value, 0),
        style: value_style(value),
        align: if is_numeric(value) {
            Align::Right
        } else {
            Align::Left
        },
    }
}

fn render_table(ctx: &RenderCtx<'_>, data: TableData) -> String {
    let mut lines = vec![border_line(
        '┌',
        '┬',
        '┐',
        '─',
        &data.widths,
        ctx.options.color,
    )];

    if ctx.options.headers {
        let header_cells: Vec<TableCell> = data
            .headers
            .into_iter()
            .map(|header| TableCell {
                text: header,
                style: Some(STYLE_BOLD),
                align: Align::Left,
            })
            .collect();
        lines.push(table_row(
            &header_cells,
            '│',
            &data.widths,
            ctx.options.color,
        ));
        lines.push(border_line(
            '├',
            '┼',
            '┤',
            '─',
            &data.widths,
            ctx.options.color,
        ));
    }

    for row in &data.rows {
        lines.push(table_row(row, '│', &data.widths, ctx.options.color));
    }
    lines.push(border_line(
        '└',
        '┴',
        '┘',
        '─',
        &data.widths,
        ctx.options.color,
    ));

    if let Some(note) = &data.note {
        lines.push(paint(note, Some(STYLE_DIM), ctx.options.color));
    }

    lines.join("\n")
}

fn border_line(
    left: char,
    middle: char,
    right: char,
    connector: char,
    widths: &[usize],
    color: bool,
) -> String {
    let mut line = String::with_capacity(widths.iter().sum::<usize>() + widths.len() * 3 + 1);
    line.push(left);
    for (index, width) in widths.iter().enumerate() {
        if index > 0 {
            line.push(middle);
        }
        for _ in 0..width + 2 {
            line.push(connector);
        }
    }
    line.push(right);
    paint(&line, Some(STYLE_DIM), color)
}

fn table_row(cells: &[TableCell], separator: char, widths: &[usize], color: bool) -> String {
    let mut row = String::from(separator);
    for (index, width) in widths.iter().enumerate() {
        let cell = cells.get(index);
        let text = cell.map(|cell| cell.text.as_str()).unwrap_or("");
        let align = cell.map(|cell| cell.align).unwrap_or(Align::Left);
        let style = cell.and_then(|cell| cell.style);
        row.push(' ');
        row.push_str(&paint(&pad(text, *width, align), style, color));
        row.push(' ');
        row.push(separator);
    }
    row
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
            let mut spacer = String::with_capacity(padding + text.len());
            spacer.extend(std::iter::repeat_n(' ', padding));
            spacer.push_str(&text);
            spacer
        }
    }
}

fn cap_items<'a>(items: &'a [Val], max_rows: usize, noun: &str) -> (&'a [Val], Option<String>) {
    if max_rows > 0 && items.len() > max_rows {
        let remaining = items.len() - max_rows;
        let plural = if remaining == 1 { "" } else { "s" };
        (
            &items[..max_rows],
            Some(format!("… {remaining} more {noun}{plural}")),
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

fn fits_width(ctx: &RenderCtx<'_>, text: &str) -> bool {
    display_width(text) <= ctx.options.width
}

fn display_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

fn truncate_display<'a>(text: impl Into<Cow<'a, str>>, max_width: usize) -> String {
    let text = text.into();
    if max_width == 0 || display_width(&text) <= max_width {
        return text.into_owned();
    }
    if max_width == 1 {
        return "…".to_string();
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
    out.push('…');
    out
}

fn paint(text: &str, style: Option<&str>, color: bool) -> String {
    match (color, style) {
        (true, Some(style)) => format!("\x1b[{style}m{text}{RESET}"),
        _ => text.to_string(),
    }
}

fn value_style(value: &Val) -> Option<&'static str> {
    match value {
        Val::Scalar(scalar) => Some(scalar_style(scalar)),
        Val::Enum(_, _) => Some(STYLE_ENUM),
        Val::None | Val::Ref { .. } => Some(STYLE_DIM),
        Val::Some(inner) => value_style(inner),
        _ => None,
    }
}

fn scalar_style(scalar: &Scalar) -> &'static str {
    match scalar {
        Scalar::Int(_) | Scalar::Float(_) | Scalar::Dec(_) => STYLE_NUMBER,
        Scalar::String(_) => STYLE_STRING,
        Scalar::Bool(_) => STYLE_BOOL,
        Scalar::Bytes(_) | Scalar::Instant(_) | Scalar::Duration(_) => STYLE_TIME,
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
    use grove_exec::storage::Key;
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
        }
    }

    fn int(value: i64) -> Val {
        Val::Scalar(Scalar::Int(value))
    }

    fn string(value: &str) -> Val {
        Val::Scalar(Scalar::String(value.to_string()))
    }

    fn user(name: &str, age: i64, active: bool) -> Val {
        Val::Record(vec![
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

    fn render_value(value: &Val, ty: &QueryType, options: &RenderOptions) -> String {
        render(value, Some(ty), &schema(), options).expect("render value")
    }

    #[test]
    fn scalar_and_optional_auto_inline() {
        let int_ty = QueryType::Scalar(ScalarType::Int);
        let optional_int = QueryType::Optional(Box::new(int_ty.clone()));

        assert_eq!(
            render_value(&int(42), &int_ty, &options(Format::Auto)),
            "42"
        );
        assert_eq!(
            render_value(&Val::None, &optional_int, &options(Format::Auto)),
            "none"
        );
        assert_eq!(
            render_value(
                &Val::Some(Box::new(int(3))),
                &optional_int,
                &options(Format::Auto)
            ),
            "some(3)"
        );
    }

    #[test]
    fn scalar_list_auto_inline() {
        let value = Val::List(vec![int(1), int(2), int(3)]);
        let ty = QueryType::List(Box::new(QueryType::Scalar(ScalarType::Int)));
        assert_eq!(
            render_value(&value, &ty, &options(Format::Auto)),
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
            render_value(&value, &ty, &options(Format::Auto)),
            "(1, \"a\")"
        );
    }

    #[test]
    fn record_auto_vertical() {
        let value = user("Alice", 30, true);
        let output = render_value(&value, &record_ty(), &options(Format::Auto));
        assert_eq!(output, "name  : \"Alice\"\nage   : 30\nactive: true");
    }

    #[test]
    fn record_list_auto_table() {
        let value = Val::List(vec![user("Alice", 30, true), user("Bob", 9, false)]);
        let output = render_value(&value, &list_record_ty(), &options(Format::Auto));
        assert!(output.contains("┌"), "{output}");
        assert!(output.contains("name"), "{output}");
        assert!(output.contains("Alice"), "{output}");
        assert!(output.contains("Bob"), "{output}");
        assert!(!output.contains(":"), "{output}");
    }

    #[test]
    fn empty_record_list_type_headers() {
        let output = render_value(
            &Val::List(Vec::new()),
            &list_record_ty(),
            &options(Format::Auto),
        );
        assert!(output.contains("┌"), "{output}");
        assert!(output.contains("name"), "{output}");
        assert!(output.contains("age"), "{output}");
    }

    #[test]
    fn wide_table_auto_vertical_fallback() {
        let value = Val::List(vec![user("Alice", 30, true), user("Bob", 9, false)]);
        let mut options = options(Format::Auto);
        options.width = 20;
        let output = render_value(&value, &list_record_ty(), &options);
        assert!(!output.contains("┌"), "{output}");
        assert!(output.contains("name  : \"Alice\""), "{output}");
    }

    #[test]
    fn table_row_cap() {
        let value = Val::List(vec![
            user("Alice", 30, true),
            user("Bob", 9, false),
            user("Cara", 41, true),
        ]);
        let mut options = options(Format::Auto);
        options.max_rows = 2;
        let output = render_value(&value, &list_record_ty(), &options);
        assert!(output.contains("1 more row"), "{output}");
        assert!(!output.contains("Cara"), "{output}");
    }

    #[test]
    fn scalar_list_row_cap() {
        let value = Val::List(vec![int(1), int(2), int(3)]);
        let ty = QueryType::List(Box::new(QueryType::Scalar(ScalarType::Int)));
        let mut options = options(Format::Auto);
        options.max_rows = 2;
        let output = render_value(&value, &ty, &options);
        assert!(output.contains("- 1"), "{output}");
        assert!(!output.contains("- 3"), "{output}");
        assert!(output.contains("1 more item"), "{output}");
    }

    #[test]
    fn table_format_non_tabular_rejected() {
        let int_ty = QueryType::Scalar(ScalarType::Int);
        assert!(render(&int(1), Some(&int_ty), &schema(), &options(Format::Table)).is_err());
    }

    #[test]
    fn color_style_scalar() {
        let int_ty = QueryType::Scalar(ScalarType::Int);
        let mut options = options(Format::Auto);
        options.color = true;
        let output = render_value(&int(7), &int_ty, &options);
        assert!(output.contains("\x1b[36m7\x1b[0m"), "{output}");
    }

    #[test]
    fn ref_fallback_without_schema_metadata() {
        let value = Val::Ref {
            field: FieldId::new(StructId::new(0), 0),
            parent: Key(7),
        };
        let output = render(&value, None, &schema(), &options(Format::Auto)).unwrap();
        assert!(output.contains("ref("), "{output}");
    }

    #[test]
    fn nested_list_auto_vertical_indentation() {
        let value = Val::List(vec![
            Val::List(vec![int(1), int(2)]),
            Val::List(vec![int(3)]),
        ]);
        let ty = QueryType::List(Box::new(QueryType::List(Box::new(QueryType::Scalar(
            ScalarType::Int,
        )))));
        let output = render_value(&value, &ty, &options(Format::Vertical));
        assert!(output.contains("-\n  - 1"), "{output}");
        assert!(output.contains("  - 3"), "{output}");
    }
}
