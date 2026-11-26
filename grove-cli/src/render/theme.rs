use std::borrow::Cow;

use grove_exec::value::{Scalar, Val};

pub(super) const ANSI_RESET: &str = "\x1b[0m";
pub(super) const ANSI_BOLD: &str = "1";
pub(super) const ANSI_DIM: &str = "2";
pub(super) const ANSI_FG_GREEN: &str = "32";
pub(super) const ANSI_FG_YELLOW: &str = "33";
pub(super) const ANSI_FG_BLUE: &str = "34";
pub(super) const ANSI_FG_MAGENTA: &str = "35";
pub(super) const ANSI_FG_CYAN: &str = "36";

pub(super) const TRIPLE_DOTS: &str = "…";
pub(super) const BOX_HORIZONTAL: &str = "─";
pub(super) const BOX_VERTICAL: &str = "│";
pub(super) const BOX_TOP_LEFT: &str = "┌";
pub(super) const BOX_TOP_CENTER: &str = "┬";
pub(super) const BOX_TOP_RIGHT: &str = "┐";
pub(super) const BOX_MIDDLE_LEFT: &str = "├";
pub(super) const BOX_MIDDLE_CENTER: &str = "┼";
pub(super) const BOX_MIDDLE_RIGHT: &str = "┤";
pub(super) const BOX_BOTTOM_LEFT: &str = "└";
pub(super) const BOX_BOTTOM_CENTER: &str = "┴";
pub(super) const BOX_BOTTOM_RIGHT: &str = "┘";

pub(super) const MIDDLE_DOT: &str = "·";
pub(super) const ARC_TOP: &str = "╭";
pub(super) const ARC_BOTTOM: &str = "╰";
pub(super) const VERTICAL_DASHED: &str = "┊";

pub(super) const BRANCH_LAST_NESTED: &str = "└─┐";
pub(super) const BRANCH_MIDDLE_NESTED: &str = "├─┐";
pub(super) const BRANCH_LAST: &str = "└─";
pub(super) const BRANCH_MIDDLE: &str = "├─";
pub(super) const RECORD_ABBREVIATION: &str = "{…}";

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

pub(super) fn scalar_style(scalar: &Scalar) -> &'static str {
    match scalar {
        Scalar::Int(_) | Scalar::Float(_) | Scalar::Dec(_) => ANSI_FG_CYAN,
        Scalar::String(_) => ANSI_FG_GREEN,
        Scalar::Bool(_) => ANSI_FG_YELLOW,
        Scalar::Bytes(_) | Scalar::Instant(_) | Scalar::Duration(_) => ANSI_FG_MAGENTA,
    }
}

pub(super) fn is_numeric(value: &Val) -> bool {
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
    use grove_exec::value::Val;
    use grove_query::QueryType;
    use grove_schema::validated::ScalarType;

    use crate::args::Format;
    use crate::render::tests::*;

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
}
