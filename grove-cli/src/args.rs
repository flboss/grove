use std::io::IsTerminal;
use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

pub use crate::backend::{BackendArgs, ConnectionSpec};

#[derive(Parser, Debug)]
#[command(
    name = "grove",
    version,
    about = "Query a tree-shaped object model over SQL storage",
    args_conflicts_with_subcommands = true,
    arg_required_else_help = true
)]
pub struct Cli {
    /// Schema file for the default REPL
    #[arg(value_name = "SCHEMA")]
    pub schema: Option<PathBuf>,

    #[command(flatten)]
    pub backend: BackendArgs,

    #[command(flatten)]
    pub output: OutputArgs,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Compile and execute one query
    Query(QueryArgs),
    /// Compile one query without opening storage
    Check(CheckArgs),
    /// Start an interactive session
    Repl(ReplArgs),
}

#[derive(Args, Debug)]
pub struct QueryArgs {
    /// Schema file
    #[arg(value_name = "SCHEMA")]
    pub schema: PathBuf,

    #[command(flatten)]
    pub input: QueryInputArgs,

    #[command(flatten)]
    pub backend: BackendArgs,

    #[command(flatten)]
    pub output: OutputArgs,

    /// Debug: Disable operation pushdown and always evaluate in Rust
    #[arg(long)]
    pub no_pushdown: bool,
}

#[derive(Args, Debug)]
pub struct CheckArgs {
    /// Schema file
    #[arg(value_name = "SCHEMA")]
    pub schema: PathBuf,

    #[command(flatten)]
    pub input: QueryInputArgs,
}

#[derive(Args, Debug)]
pub struct ReplArgs {
    /// Schema file
    #[arg(value_name = "SCHEMA")]
    pub schema: PathBuf,

    #[command(flatten)]
    pub backend: BackendArgs,

    #[command(flatten)]
    pub output: OutputArgs,

    /// Debug: Disable operation pushdown and always evaluate in Rust
    #[arg(long)]
    pub no_pushdown: bool,
}

#[derive(Args, Debug)]
pub struct QueryInputArgs {
    /// Query source, or `-` to read stdin
    #[arg(value_name = "QUERY")]
    pub query: Option<String>,

    /// Read query source from a file
    #[arg(long, value_name = "PATH", conflicts_with = "query")]
    pub query_file: Option<PathBuf>,
}

#[derive(Args, Debug, Clone)]
pub struct OutputArgs {
    /// Result format
    #[arg(long, value_enum, default_value = "auto")]
    pub format: Format,

    /// Color output
    #[arg(long, value_enum, default_value = "auto")]
    pub color: ColorChoice,

    /// Render width (defaults to the terminal width)
    #[arg(long, value_name = "N")]
    pub width: Option<usize>,

    /// Maximum rows to render (0 = unlimited)
    #[arg(long, value_name = "N", default_value_t = 0)]
    pub max_rows: usize,

    /// Maximum nested depth to render
    #[arg(long, value_name = "N", default_value_t = 6)]
    pub max_depth: usize,

    /// Maximum cell width before truncation
    #[arg(long, value_name = "N", default_value_t = 60)]
    pub max_cell_width: usize,

    /// Disable depth and cell truncation
    #[arg(long)]
    pub full: bool,

    /// Omit table headers
    #[arg(long)]
    pub no_headers: bool,

    /// Hide unresolved non-owning reference values in output
    #[arg(long)]
    pub hide_refs: bool,

    /// Write results to a file instead of stdout
    #[arg(long, value_name = "PATH")]
    pub output: Option<PathBuf>,

    /// Print phase timings to stderr
    #[arg(long)]
    pub stats: bool,
}

impl OutputArgs {
    pub fn color_enabled(&self) -> bool {
        match self.color {
            ColorChoice::Always => true,
            ColorChoice::Never => false,
            ColorChoice::Auto => {
                self.output.is_none()
                    && std::io::stdout().is_terminal()
                    && std::env::var_os("NO_COLOR").is_none()
            }
        }
    }
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Auto,
    Inline,
    Raw,
    Table,
    Vertical,
}

impl Format {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "auto" => Some(Format::Auto),
            "inline" => Some(Format::Inline),
            "raw" => Some(Format::Raw),
            "table" => Some(Format::Table),
            "vertical" => Some(Format::Vertical),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Format::Auto => "auto",
            Format::Inline => "inline",
            Format::Raw => "raw",
            Format::Table => "table",
            Format::Vertical => "vertical",
        }
    }
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorChoice {
    Auto,
    Always,
    Never,
}
