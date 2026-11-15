use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

const SCHEMA: &str = "\
root users: User;
struct User {
    id: Int,
    name: String,
}
";

const DB_INIT: &str = "\
CREATE TABLE users (id INTEGER NOT NULL, name TEXT NOT NULL);
INSERT INTO users (id, name) VALUES (1, 'Alice'), (2, 'Bob');
";

static NEXT_FIXTURE: AtomicUsize = AtomicUsize::new(0);

fn grove() -> Command {
    Command::new(env!("CARGO_BIN_EXE_grove"))
}

fn fixture(name: &str) -> PathBuf {
    let id = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("grove-cli-{name}-{}-{id}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create fixture directory");
    dir
}

fn write_schema(dir: &Path) -> PathBuf {
    let path = dir.join("cli-test.gschema");
    fs::write(&path, SCHEMA).expect("write schema");
    path
}

fn write_db(dir: &Path) -> PathBuf {
    let path = dir.join("data.sqlite3");
    let conn = rusqlite::Connection::open(&path).expect("open fixture database");
    conn.execute_batch(DB_INIT).expect("seed fixture database");
    path
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn status(output: &Output) -> i32 {
    output
        .status
        .code()
        .expect("process should exit with a code")
}

#[test]
fn query_print_result_to_stdout() {
    let dir = fixture("query");
    let schema = write_schema(&dir);
    let db = write_db(&dir);

    let output = grove()
        .arg("query")
        .arg(&schema)
        .arg("users { name }")
        .arg("--file")
        .arg(&db)
        .output()
        .expect("run grove");

    assert_eq!(status(&output), 0, "stderr: {}", stderr(&output));
    assert!(stdout(&output).contains("Alice"), "{}", stdout(&output));
    assert!(stderr(&output).is_empty(), "{}", stderr(&output));
}

#[test]
fn query_read_query_file() {
    let dir = fixture("query-file");
    let schema = write_schema(&dir);
    let db = write_db(&dir);
    let query = dir.join("query.grv");
    fs::write(&query, "users { name }").expect("write query");

    let output = grove()
        .arg("query")
        .arg(&schema)
        .arg("--query-file")
        .arg(&query)
        .arg("--file")
        .arg(&db)
        .output()
        .expect("run grove");

    assert_eq!(status(&output), 0, "stderr: {}", stderr(&output));
    assert!(stdout(&output).contains("Alice"));
}

#[test]
fn query_read_stdin() {
    let dir = fixture("stdin");
    let schema = write_schema(&dir);
    let db = write_db(&dir);

    let mut child = grove()
        .arg("query")
        .arg(&schema)
        .arg("-")
        .arg("--file")
        .arg(&db)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn grove");

    child
        .stdin
        .as_mut()
        .expect("stdin pipe")
        .write_all(b"users { name }")
        .expect("write query to stdin");

    let output = child.wait_with_output().expect("wait for grove");
    assert_eq!(status(&output), 0, "stderr: {}", stderr(&output));
    assert!(stdout(&output).contains("Alice"));
}

#[test]
fn query_write_output_file() {
    let dir = fixture("output");
    let schema = write_schema(&dir);
    let db = write_db(&dir);
    let result = dir.join("result.txt");

    let output = grove()
        .arg("query")
        .arg(&schema)
        .arg("users { name }")
        .arg("--file")
        .arg(&db)
        .arg("--output")
        .arg(&result)
        .output()
        .expect("run grove");

    assert_eq!(status(&output), 0, "stderr: {}", stderr(&output));
    assert!(stdout(&output).is_empty(), "{}", stdout(&output));
    let written = fs::read_to_string(&result).expect("read result");
    assert!(written.contains("Alice"), "{written}");
}

#[test]
fn query_readonly_database() {
    let dir = fixture("readonly");
    let schema = write_schema(&dir);
    let db = write_db(&dir);

    let output = grove()
        .arg("query")
        .arg(&schema)
        .arg("users { name }")
        .arg("--file")
        .arg(&db)
        .arg("--readonly")
        .output()
        .expect("run grove");

    assert_eq!(status(&output), 0, "stderr: {}", stderr(&output));
}

#[test]
fn query_stats_stderr() {
    let dir = fixture("stats");
    let schema = write_schema(&dir);
    let db = write_db(&dir);

    let output = grove()
        .arg("query")
        .arg(&schema)
        .arg("users { name }")
        .arg("--file")
        .arg(&db)
        .arg("--stats")
        .output()
        .expect("run grove");

    assert_eq!(status(&output), 0, "stderr: {}", stderr(&output));
    assert!(
        stderr(&output).contains(" ms, compile: "),
        "stderr: {}",
        stderr(&output)
    );
}

#[test]
fn check_success() {
    let dir = fixture("check");
    let schema = write_schema(&dir);

    let output = grove()
        .arg("check")
        .arg(&schema)
        .arg("users { name }")
        .output()
        .expect("run grove");

    assert_eq!(status(&output), 0, "stderr: {}", stderr(&output));
    assert!(stdout(&output).is_empty());
    assert!(stderr(&output).is_empty(), "{}", stderr(&output));
}

#[test]
fn check_backend_flags_rejected() {
    let dir = fixture("check-backend");
    let schema = write_schema(&dir);
    let db = write_db(&dir);

    let output = grove()
        .arg("check")
        .arg(&schema)
        .arg("users { name }")
        .arg("--file")
        .arg(&db)
        .output()
        .expect("run grove");

    assert_eq!(status(&output), 2);
    assert!(stderr(&output).contains("--file"), "{}", stderr(&output));
}

#[test]
fn compile_error_exit_code_stderr() {
    let dir = fixture("compile-error");
    let schema = write_schema(&dir);

    let output = grove()
        .arg("check")
        .arg(&schema)
        .arg("users {")
        .output()
        .expect("run grove");

    assert_eq!(status(&output), 3);
    assert!(stdout(&output).is_empty());
    assert!(!stderr(&output).is_empty());
    assert!(
        stderr(&output).contains("QP") || stderr(&output).contains("expected"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn missing_backend_file_exit_code() {
    let dir = fixture("missing-file");
    let schema = write_schema(&dir);

    let output = grove()
        .arg("query")
        .arg(&schema)
        .arg("users { name }")
        .output()
        .expect("run grove");

    assert_eq!(status(&output), 2);
    assert!(
        stderr(&output).contains("--file is required"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn missing_schema_exit_code() {
    let dir = fixture("missing-schema");
    let missing = dir.join("missing.gschema");

    let output = grove()
        .arg("check")
        .arg(&missing)
        .arg("users { name }")
        .output()
        .expect("run grove");

    assert_eq!(status(&output), 1);
    assert!(
        stderr(&output).contains("schema file not found"),
        "{}",
        stderr(&output)
    );
}
