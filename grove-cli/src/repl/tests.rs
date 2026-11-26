use std::fs;
use std::path::PathBuf;

use grove_exec::SqliteBackend;
use grove_exec::value::{Scalar, Val};
use grove_exec::{ExecOptions, ExecSession};
use grove_query::TypeBindings;
use grove_schema::ValidatedSchema;

use super::{ReplOutcome, ReplSession, ReplSettings};
use crate::args::{ColorChoice, ConnectionSpec, Format, OutputArgs};

fn settings() -> ReplSettings {
    ReplSettings {
        output: OutputArgs {
            format: Format::Auto,
            color: ColorChoice::Never,
            width: Some(80),
            max_rows: 0,
            max_depth: 6,
            max_cell_width: 40,
            full: false,
            no_headers: false,
            hide_refs: false,
            output: None,
            stats: false,
        },
        stats: false,
    }
}

fn spec() -> ConnectionSpec {
    ConnectionSpec::Sqlite {
        file: PathBuf::from("<test>"),
        readonly: false,
    }
}

fn session<'a>(
    schema: &'a ValidatedSchema,
    spec: &'a ConnectionSpec,
    backend: &'a SqliteBackend<'a>,
    settings: &'a mut ReplSettings,
    turn: &'a mut usize,
) -> ReplSession<'a> {
    ReplSession {
        schema,
        spec,
        schema_path: PathBuf::from("<test>"),
        exec: ExecSession::new(backend, schema),
        types: TypeBindings::new(),
        settings,
        exec_options: ExecOptions {
            pushdown: true,
            materialize_lets: true,
            read_only: false,
            debug_rollback: false,
        },
        turn,
    }
}

#[test]
fn bindings_persistence_across_turns() {
    let schema = ValidatedSchema::default();
    let backend = SqliteBackend::open_in_memory(&schema).expect("backend");
    let mut settings = settings();
    let spec = spec();
    let mut turn = 0;
    let mut session = session(&schema, &spec, &backend, &mut settings, &mut turn);

    assert_eq!(
        session.execute_source("let x = 41;", "<test:1>").unwrap(),
        None
    );
    assert!(session.types.get("x").is_some());
    assert_eq!(
        session.execute_source("x + 1", "<test:2>").unwrap(),
        Some(Val::Scalar(Scalar::Int(42)))
    );
}

#[test]
fn persisted_let_all_fields_kept() {
    let source = "root users: User; struct User { name: String, age: Int }";
    let (schema, diagnostics) = grove_schema::parse_schema(source);
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
    let (schema, diagnostics) = grove_schema::validate(schema.unwrap());
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
    let schema = schema.unwrap();

    let conn = rusqlite::Connection::open_in_memory().expect("in-memory database");
    conn.execute_batch(
        "CREATE TABLE users (name TEXT NOT NULL, age INTEGER NOT NULL);\
         INSERT INTO users (name, age) VALUES ('Alice', 30);",
    )
    .expect("seed users");
    let backend = SqliteBackend::new(conn, &schema).expect("backend");
    let mut settings = settings();
    let spec = spec();
    let mut turn = 0;
    let mut session = session(&schema, &spec, &backend, &mut settings, &mut turn);

    assert_eq!(
        session
            .execute_source("let a = users.first();", "<test:1>")
            .unwrap(),
        None
    );
    let value = session
        .execute_source("a", "<test:2>")
        .unwrap()
        .expect("record");
    let Val::Some(inner) = value else {
        panic!("expected some(record), got {value:?}");
    };
    let Val::Record(fields) = inner.as_ref() else {
        panic!("expected record, got {inner:?}");
    };
    assert!(fields.iter().any(|(name, value)| {
        name.as_ref() == "name" && *value == Val::Scalar(Scalar::String("Alice".into()))
    }));
    assert!(
        fields.iter().any(|(name, value)| {
            name.as_ref() == "age" && *value == Val::Scalar(Scalar::Int(30))
        })
    );
}

#[test]
fn clear_bindings_types_and_values() {
    let schema = ValidatedSchema::default();
    let backend = SqliteBackend::open_in_memory(&schema).expect("backend");
    let mut settings = settings();
    let spec = spec();
    let mut turn = 0;
    let mut session = session(&schema, &spec, &backend, &mut settings, &mut turn);

    session.execute_source("let x = 1;", "<test:1>").unwrap();
    session.meta_command(".clear").unwrap();
    assert!(session.types.is_empty());
    assert_eq!(session.exec.bindings().count(), 0);
}

#[test]
fn unlet_remove_binding() {
    let schema = ValidatedSchema::default();
    let backend = SqliteBackend::open_in_memory(&schema).expect("backend");
    let mut settings = settings();
    let spec = spec();
    let mut turn = 0;
    let mut session = session(&schema, &spec, &backend, &mut settings, &mut turn);

    session.execute_source("let x = 1;", "<test:1>").unwrap();
    session.execute_source("let y = 2;", "<test:2>").unwrap();
    session.meta_command(".unlet x").unwrap();
    assert!(session.types.get("x").is_none());
    assert!(session.exec.binding("x").is_none());
    assert!(session.types.get("y").is_some());
}

#[test]
fn vars_empty_contexts() {
    let schema = ValidatedSchema::default();
    let backend = SqliteBackend::open_in_memory(&schema).expect("backend");
    let mut settings = settings();
    let spec = spec();
    let mut turn = 0;
    let mut session = session(&schema, &spec, &backend, &mut settings, &mut turn);

    session.execute_source("let x = 1;", "<test:1>").unwrap();
    session.meta_command(".vars").unwrap();
    session.meta_command(".clear").unwrap();
    session.meta_command(".vars").unwrap();
}

#[test]
fn read_execute_file_in_session() {
    let schema = ValidatedSchema::default();
    let backend = SqliteBackend::open_in_memory(&schema).expect("backend");
    let mut settings = settings();
    let spec = spec();
    let mut turn = 0;
    let mut session = session(&schema, &spec, &backend, &mut settings, &mut turn);

    let path = std::env::temp_dir().join(format!("grove-repl-read-{}.grv", std::process::id()));
    fs::write(&path, "let y = 2;").expect("write query file");
    session
        .meta_command(&format!(".read {}", path.display()))
        .unwrap();
    let _ = fs::remove_file(&path);
    assert!(session.types.get("y").is_some());
}

#[test]
fn failed_turn_no_bindings_committed() {
    let schema = ValidatedSchema::default();
    let backend = SqliteBackend::open_in_memory(&schema).expect("backend");
    let mut settings = settings();
    let spec = spec();
    let mut turn = 0;
    let mut session = session(&schema, &spec, &backend, &mut settings, &mut turn);

    session.execute_source("let x = 1;", "<test:1>").unwrap();
    assert_eq!(
        session
            .execute_source("let y = 2; 1 / 0", "<test:2>")
            .unwrap(),
        None
    );
    assert!(session.types.get("x").is_some());
    assert!(session.types.get("y").is_none());
}

#[test]
fn meta_commands_render_settings() {
    let schema = ValidatedSchema::default();
    let backend = SqliteBackend::open_in_memory(&schema).expect("backend");
    let mut settings = settings();
    let spec = spec();
    let mut turn = 0;
    let mut session = session(&schema, &spec, &backend, &mut settings, &mut turn);

    session.meta_command(".format vertical").unwrap();
    session.meta_command(".headers off").unwrap();
    session.meta_command(".width 42").unwrap();
    session.meta_command(".maxrows 5").unwrap();
    session.meta_command(".hiderefs on").unwrap();

    assert_eq!(session.settings.output.format, Format::Vertical);
    assert!(session.settings.output.no_headers);
    assert_eq!(session.settings.output.width, Some(42));
    assert_eq!(session.settings.output.max_rows, 5);
    assert!(session.settings.output.hide_refs);
}

#[test]
fn prompt_sqlite_backend_aligned_continuation() {
    let schema = ValidatedSchema::default();
    let backend = SqliteBackend::open_in_memory(&schema).expect("backend");
    let spec = ConnectionSpec::Sqlite {
        file: PathBuf::from("/tmp/db.sqlite3"),
        readonly: false,
    };
    let mut settings = settings();
    let mut turn = 0;

    let session = ReplSession {
        schema: &schema,
        spec: &spec,
        schema_path: PathBuf::from("/tmp/schema.gschema"),
        exec: ExecSession::new(&backend, &schema),
        types: TypeBindings::new(),
        settings: &mut settings,
        exec_options: ExecOptions::default(),
        turn: &mut turn,
    };

    let (full, continuation) = session.prompts();
    assert_eq!(full, "grove schema.gschema SQLite: db.sqlite3 > ");
    assert_eq!(full.find('>'), continuation.find('>'));
}

#[test]
fn prompt_file_names_truncated() {
    let schema = ValidatedSchema::default();
    let backend = SqliteBackend::open_in_memory(&schema).expect("backend");
    let spec = ConnectionSpec::Memory;
    let mut settings = settings();
    let mut turn = 0;
    let long_schema = std::env::temp_dir().join(format!("{}-schema.gschema", "x".repeat(40)));
    let session = ReplSession {
        schema: &schema,
        spec: &spec,
        schema_path: long_schema,
        exec: ExecSession::new(&backend, &schema),
        types: TypeBindings::new(),
        settings: &mut settings,
        exec_options: ExecOptions::default(),
        turn: &mut turn,
    };

    let (full, _) = session.prompts();
    assert!(full.contains("in-memory"), "{full}");
    assert!(full.contains('…'), "{full}");
    assert!(!full.contains(&"x".repeat(40)), "{full}");
}

#[test]
fn schema_command_load_new_path() {
    let schema = ValidatedSchema::default();
    let backend = SqliteBackend::open_in_memory(&schema).expect("backend");
    let mut settings = settings();
    let spec = spec();
    let mut turn = 0;
    let mut session = session(&schema, &spec, &backend, &mut settings, &mut turn);

    let path =
        std::env::temp_dir().join(format!("grove-repl-schema-{}.gschema", std::process::id()));
    fs::write(&path, "root items: Item; struct Item { id: Int }").expect("write schema");
    let outcome = session
        .meta_command(&format!(".schema {}", path.display()))
        .unwrap()
        .expect("reload outcome");
    let _ = fs::remove_file(&path);

    match outcome {
        ReplOutcome::ReloadSchema {
            path: actual_path, ..
        } => assert_eq!(actual_path, path),
        other => panic!("expected schema reload, got {other:?}"),
    }
}

#[test]
fn open_command_accepts_schema() {
    let schema = ValidatedSchema::default();
    let backend = SqliteBackend::open_in_memory(&schema).expect("backend");
    let mut settings = settings();
    let spec = ConnectionSpec::Memory;
    let mut turn = 0;
    let mut session = session(&schema, &spec, &backend, &mut settings, &mut turn);

    let schema_path =
        std::env::temp_dir().join(format!("grove-repl-open-{}.gschema", std::process::id()));
    fs::write(&schema_path, "root items: Item; struct Item { id: Int }").expect("write schema");
    let db = std::env::current_dir().unwrap().join("Cargo.toml");
    let outcome = session
        .meta_command(&format!(".open {} {}", db.display(), schema_path.display()))
        .unwrap()
        .expect("open outcome");
    let _ = fs::remove_file(&schema_path);

    match outcome {
        ReplOutcome::ChangeBackend {
            spec: ConnectionSpec::Sqlite { .. },
            schema: Some((path, _)),
        } => assert_eq!(path, schema_path),
        other => panic!("expected backend change, got {other:?}"),
    }
}

#[test]
fn close_command_switch_to_memory() {
    let schema = ValidatedSchema::default();
    let backend = SqliteBackend::open_in_memory(&schema).expect("backend");
    let mut settings = settings();
    let spec = spec();
    let mut turn = 0;
    let mut session = session(&schema, &spec, &backend, &mut settings, &mut turn);

    let outcome = session
        .meta_command(".close")
        .unwrap()
        .expect("close outcome");
    match outcome {
        ReplOutcome::ChangeBackend {
            spec: ConnectionSpec::Memory,
            schema: None,
        } => {}
        other => panic!("expected memory backend, got {other:?}"),
    }
}
