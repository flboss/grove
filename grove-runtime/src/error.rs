
use std::fmt;

#[derive(Debug)]
pub enum RuntimeError {
    Prepare { index: usize, message: String },
    Bind { index: usize, message: String },
    Step { index: usize, message: String },
    Decode {
        column: String,
        expected: String,
        found: String,
    },
    Aborted(String),
    Contract(String),
    Empty,
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RuntimeError::Prepare { index, message } => {
                write!(f, "statement {index} failed to prepare: {message}")
            }
            RuntimeError::Bind { index, message } => {
                write!(f, "statement {index} failed to bind params: {message}")
            }
            RuntimeError::Step { index, message } => {
                write!(f, "statement {index} failed to step: {message}")
            }
            RuntimeError::Decode {
                column,
                expected,
                found,
            } => {
                write!(f, "column {column}: expected {expected}, found {found}")
            }
            RuntimeError::Aborted(message) => write!(f, "query aborted: {message}"),
            RuntimeError::Contract(message) => write!(f, "plan contract violated: {message}"),
            RuntimeError::Empty => write!(f, "query produced no rows where one was required"),
        }
    }
}

impl std::error::Error for RuntimeError {}

impl RuntimeError {
    pub fn step_error(index: usize, err: rusqlite::Error) -> Self {
        let message = err.to_string();
        if let Some(at) = message.find("grove-abort: ") {
            return RuntimeError::Aborted(message[at + "grove-abort: ".len()..].to_string());
        }
        RuntimeError::Step { index, message }
    }
}
