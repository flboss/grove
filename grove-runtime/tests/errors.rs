
use grove_runtime::RuntimeError;

#[test]
fn error_display() {
    assert_eq!(
        RuntimeError::Prepare {
            index: 2,
            message: "no such table".to_string()
        }
        .to_string(),
        "statement 2 failed to prepare: no such table"
    );
    assert_eq!(
        RuntimeError::Bind {
            index: 0,
            message: "x".to_string()
        }
        .to_string(),
        "statement 0 failed to bind params: x"
    );
    assert_eq!(
        RuntimeError::Step {
            index: 1,
            message: "y".to_string()
        }
        .to_string(),
        "statement 1 failed to step: y"
    );
    assert_eq!(
        RuntimeError::Decode {
            column: "c".to_string(),
            expected: "Int".to_string(),
            found: "TEXT".to_string(),
        }
        .to_string(),
        "column c: expected Int, found TEXT"
    );
    assert_eq!(
        RuntimeError::Aborted("boom".to_string()).to_string(),
        "query aborted: boom"
    );
    assert_eq!(
        RuntimeError::Contract("z".to_string()).to_string(),
        "plan contract violated: z"
    );
    assert_eq!(
        RuntimeError::Empty.to_string(),
        "query produced no rows where one was required"
    );
}

#[test]
fn step_error_recognizes_abort_marker() {
    let sqlite = rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(1),
        Some("grove-abort: take needs non-negative".to_string()),
    );
    assert_eq!(
        RuntimeError::step_error(0, sqlite),
        RuntimeError::Aborted("take needs non-negative".to_string())
    );
    let sqlite = rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(1),
        Some("no such table: users".to_string()),
    );
    assert!(matches!(
        RuntimeError::step_error(3, sqlite),
        RuntimeError::Step { index: 3, .. }
    ));
}
