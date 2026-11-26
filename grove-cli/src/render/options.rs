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
