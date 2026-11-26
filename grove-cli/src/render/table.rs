use grove_exec::value::Val;
use grove_query::{QueryType, RecordSource};
use grove_schema::ValidatedSchema;

use crate::render::RenderCtx;
use crate::render::inline::cell_styled;
use crate::render::text::{Align, Styled, cap_items, display_width, pad_styled, truncate_display};
use crate::render::theme::{
    ANSI_BOLD, ANSI_DIM, BOX_BOTTOM_CENTER, BOX_BOTTOM_LEFT, BOX_BOTTOM_RIGHT, BOX_HORIZONTAL,
    BOX_MIDDLE_CENTER, BOX_MIDDLE_LEFT, BOX_MIDDLE_RIGHT, BOX_TOP_CENTER, BOX_TOP_LEFT,
    BOX_TOP_RIGHT, BOX_VERTICAL, is_numeric, paint,
};

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

pub(super) struct TableData {
    headers: Vec<String>,
    rows: Vec<Vec<TableCell>>,
    widths: Vec<usize>,
    note: Option<String>,
}

pub(super) enum TableError {
    NotTabular,
    TooWide,
}

pub(super) fn build_table(
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

pub(super) fn render_table(ctx: &RenderCtx<'_>, data: TableData) -> String {
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

#[cfg(test)]
mod tests {
    use grove_exec::value::Val;
    use grove_query::QueryType;
    use grove_schema::validated::ScalarType;

    use crate::args::Format;
    use crate::render::render;
    use crate::render::tests::*;

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
}
