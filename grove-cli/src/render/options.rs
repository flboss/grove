use crate::args::{Format, OutputArgs};

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

#[cfg(test)]
mod tests {
    use std::rc::Rc;

    use grove_exec::value::{Key, Val};
    use grove_schema::validated::{FieldId, StructId};

    use crate::args::Format;
    use crate::render::render;
    use crate::render::tests::*;

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
    fn hide_refs_fields_and_list_items_omitted() {
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
        assert!(!output.contains("ref("), "{output}");
        assert!(!output.contains("&"), "{output}");

        let mut list = Val::list(vec![reference, int(1)]);
        let mut opts = options(Format::Auto);
        opts.hide_refs = true;
        let output = render(&mut list, None, &schema, &opts).unwrap();
        assert_eq!(output, "[1]");
    }

    #[test]
    fn hide_refs_table_columns_removed() {
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
}
