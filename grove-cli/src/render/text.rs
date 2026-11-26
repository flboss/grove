use std::borrow::Cow;

use grove_exec::value::Val;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::render::theme::{ANSI_RESET, TRIPLE_DOTS, paint};

#[derive(Clone, Default)]
pub(super) struct Styled {
    pub(super) styled: String,
    pub(super) width: usize,
}

impl Styled {
    pub(super) fn painted<'a>(text: impl Into<Cow<'a, str>>, style: &str, color: bool) -> Self {
        let text = text.into();
        let width = display_width(&text);
        let styled = paint(text, Some(style), color).into_owned();
        Styled { styled, width }
    }
}

#[derive(Clone, Copy)]
pub(super) enum Align {
    Left,
    Right,
}

pub(super) fn pad_styled(content: &Styled, width: usize, align: Align) -> String {
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

pub(super) fn pad<'a>(text: impl Into<Cow<'a, str>>, width: usize, align: Align) -> String {
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

pub(super) fn cap_items<'a>(
    items: &'a [Val],
    max_rows: usize,
    noun: &str,
) -> (&'a [Val], Option<String>) {
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

pub(super) fn display_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

pub(super) fn truncate_display<'a>(
    text: impl Into<Cow<'a, str>>,
    max_width: usize,
) -> Cow<'a, str> {
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

pub(super) fn truncate_styled(styled: Styled, max_width: usize) -> Styled {
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
