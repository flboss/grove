/// Successful completion
pub(crate) const EXIT_SUCCESS: i32 = 0;
/// Runtime, storage, or I/O failure
pub(crate) const EXIT_FAILURE: i32 = 1;
/// Invalid CLI usage
pub(crate) const EXIT_USAGE: i32 = 2;
/// Schema or query compilation diagnostics
pub(crate) const EXIT_COMPILE: i32 = 3;

#[derive(Debug)]
pub(crate) struct CliError {
    pub(crate) code: i32,
    pub(crate) message: Option<String>,
}

impl CliError {
    /// Reports an already-rendered failure without adding another message.
    pub(crate) fn reported(code: i32) -> Self {
        CliError {
            code,
            message: None,
        }
    }

    pub(crate) fn usage(message: impl Into<String>) -> Self {
        CliError {
            code: EXIT_USAGE,
            message: Some(message.into()),
        }
    }

    pub(crate) fn runtime(message: impl Into<String>) -> Self {
        CliError {
            code: EXIT_FAILURE,
            message: Some(message.into()),
        }
    }
}

pub(crate) type CliResult<T> = Result<T, CliError>;
