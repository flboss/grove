use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

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

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Compile and execute one query
    Query(QueryArgs),
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

    /// Debug: Disable operation pushdown and always evaluate in Rust
    #[arg(long)]
    pub no_pushdown: bool,
}

#[derive(Args, Debug)]
pub struct ReplArgs {
    /// Schema file
    #[arg(value_name = "SCHEMA")]
    pub schema: PathBuf,

    #[command(flatten)]
    pub backend: BackendArgs,

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
pub struct BackendArgs {
    /// Storage backend
    #[arg(long, value_enum, default_value = "sqlite")]
    pub backend: BackendKind,

    /// Database file (required for the sqlite backend)
    #[arg(long, alias = "db", value_name = "PATH")]
    pub file: Option<PathBuf>,

    /// Open the database read-only
    #[arg(long)]
    pub readonly: bool,
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackendKind {
    Sqlite,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionSpec {
    Sqlite { file: PathBuf, readonly: bool },
}

impl BackendArgs {
    pub fn resolve(&self) -> Result<ConnectionSpec, String> {
        match self.backend {
            BackendKind::Sqlite => {
                let file = self
                    .file
                    .clone()
                    .ok_or_else(|| "--file is required for --backend sqlite".to_string())?;
                Ok(ConnectionSpec::Sqlite {
                    file,
                    readonly: self.readonly,
                })
            }
        }
    }
}
