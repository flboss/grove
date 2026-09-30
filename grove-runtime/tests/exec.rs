
use chrono::{DateTime, TimeDelta};
use grove_query::codegen::model::{CompiledQuery, Param, StatementKind};
use grove_runtime::decode::{encode_dec, encode_duration, encode_instant};
use grove_runtime::{Runtime, RuntimeError, Value};
use rust_decimal::Decimal;
use std::str::FromStr;

const SCHEMA_SRC: &str = r#"
    root users: User;
    struct User {
        name: String,
        age: Int,
        balance: Dec,
        active: Bool,
        created: Instant,
        ttl: Duration,
        orders: List<Order>,
    }
    struct Order {
        total: Dec,
        user: &User,
    }
    rel Order.user <<-> User.orders (orders.user_id -> users.id);
"#;

fn compile(query: &str) -> CompiledQuery {
    let schema = grove_schema::validate(
        grove_schema::parse_schema(SCHEMA_SRC)
            .0
            .expect("invalid test schema"),
    )
    .0
    .expect("invalid test schema");
    let (file, parse_diags) = grove_query::parse_query(query);
    assert!(parse_diags.is_empty());
    let (typed, type_diags) = grove_query::typecheck::typecheck(file.unwrap(), &schema);
    assert!(type_diags.is_empty());
    grove_query::codegen::codegen(&typed.unwrap(), &schema)
}

fn dec(s: &str) -> Decimal {
    Decimal::from_str(s).unwrap()
}

fn fixture() -> Runtime {
    let rt = Runtime::open_in_memory().unwrap();
    rt.conn()
        .execute_batch(
            "CREATE TABLE users(id INTEGER, name TEXT, age INTEGER, balance BLOB, active INTEGER, created BLOB, ttl BLOB);
             CREATE TABLE orders(id INTEGER, total BLOB, user_id INTEGER);",
        )
        .unwrap();
    let created = encode_instant(&DateTime::from_timestamp(1_700_000_000, 0).unwrap());
    let ttl = encode_duration(&TimeDelta::seconds(3600));
    let users: &[(i64, &str, i64, &str, i64)] =
        &[(1, "Ada", 30, "100.50", 1), (2, "Bob", 40, "200.25", 0)];
    for (id, name, age, balance, active) in users {
        rt.conn()
            .execute(
                "INSERT INTO users(id, name, age, balance, active, created, ttl) VALUES (?, ?, ?, ?, ?, ?, ?)",
                rusqlite::params![
                    id,
                    name,
                    age,
                    encode_dec(&dec(balance)).to_vec(),
                    active,
                    created.to_vec(),
                    ttl.to_vec(),
                ],
            )
            .unwrap();
    }
    let orders: &[(i64, &str, i64)] = &[(10, "10.00", 1), (11, "20.00", 1), (12, "30.00", 2)];
    for (id, total, user_id) in orders {
        rt.conn()
            .execute(
                "INSERT INTO orders(id, total, user_id) VALUES (?, ?, ?)",
                rusqlite::params![id, encode_dec(&dec(total)).to_vec(), user_id],
            )
            .unwrap();
    }
    rt
}

fn names(rt: &mut Runtime, query: &str) -> Vec<String> {
    let compiled = compile(query);
    let outputs = rt.execute(&compiled).unwrap();
    outputs[0]
        .rows
        .iter()
        .map(|row| match row.get("name") {
            Some(Value::String(s)) => s.clone(),
            other => panic!("expected name string, got {other:?}"),
        })
        .collect()
}

#[test]
fn flat_rows_decode() {
    let mut rt = fixture();
    assert_eq!(names(&mut rt, "users { name }"), vec!["Ada", "Bob"]);
}

#[test]
fn filter_param_binds() {
    let mut rt = fixture();
    assert_eq!(names(&mut rt, "users[age > 30] { name }"), vec!["Bob"]);
}

#[test]
fn bool_decodes_by_type() {
    let mut rt = fixture();
    let compiled = compile("users.take(1) { active }");
    let outputs = rt.execute(&compiled).unwrap();
    assert_eq!(outputs[0].rows[0].get("active"), Some(&Value::Bool(true)));
}

#[test]
fn dec_round_trips_normalized() {
    let mut rt = fixture();
    let compiled = compile("users[age > 30] { balance }");
    let outputs = rt.execute(&compiled).unwrap();
    let Value::Dec(d) = outputs[0].rows[0].get("balance").unwrap() else {
        panic!("expected dec");
    };
    assert_eq!(d.to_string(), "200.25");
}

#[test]
fn keyed_children_pair_by_bulk() {
    let mut rt = fixture();
    let compiled = compile("users { name, o = orders { total } }");
    assert_eq!(compiled.statements.len(), 2);
    let outputs = rt.execute(&compiled).unwrap();
    assert_eq!(outputs[0].rows.len(), 2);
    assert_eq!(outputs[1].rows.len(), 3);
    let ada_key = outputs[0].rows[0].get("id");
    assert_eq!(ada_key, Some(&Value::Int(1)));
}

#[test]
fn empty_parent_empties_children() {
    let mut rt = fixture();
    let compiled = compile("users[age > 100] { name, o = orders { total } }");
    let outputs = rt.execute(&compiled).unwrap();
    assert!(outputs[0].rows.is_empty());
    assert!(outputs[1].rows.is_empty());
}

#[test]
fn temp_lane_materializes_and_cleans_up() {
    let mut rt = fixture();
    let compiled = compile("users { name, big = orders.filter(total > balance) { total } }");
    assert!(
        compiled
            .statements
            .iter()
            .any(|s| matches!(s.kind, StatementKind::Temp { .. })),
        "expected a temp statement"
    );
    let outputs = rt.execute(&compiled).unwrap();
    assert_eq!(outputs[0].rows.len(), 2);
    let leftovers: Vec<String> = rt
        .conn()
        .prepare("SELECT name FROM sqlite_temp_master WHERE type = 'table'")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert!(leftovers.is_empty(), "temps leaked: {leftovers:?}");
}

#[test]
fn literals_serve_rows() {
    let mut rt = fixture();
    let compiled = compile("[10, 20]");
    let outputs = rt.execute(&compiled).unwrap();
    let got: Vec<i64> = outputs[0]
        .rows
        .iter()
        .map(|row| match row.get("c0") {
            Some(Value::Int(n)) => *n,
            other => panic!("expected int, got {other:?}"),
        })
        .collect();
    assert_eq!(got, vec![10, 20]);
    let outputs = rt.execute(&compiled).unwrap();
    assert_eq!(outputs[0].rows.len(), 2);
}

#[test]
fn unknown_literal_id_is_contract_error() {
    let mut rt = fixture();
    let mut compiled = compile("[10, 20]");
    compiled.literals.clear();
    let err = rt.execute(&compiled).unwrap_err();
    assert!(
        matches!(err, RuntimeError::Contract(_)),
        "unexpected: {err:?}"
    );
}

#[test]
fn panic_aborts() {
    let mut rt = fixture();
    let compiled = compile("users.take(-1) { name }");
    let err = rt.execute(&compiled).unwrap_err();
    assert!(
        matches!(err, RuntimeError::Aborted(_)),
        "unexpected: {err:?}"
    );
}

#[test]
fn missing_table_is_prepare_error() {
    let mut rt = Runtime::open_in_memory().unwrap();
    let compiled = compile("users { name }");
    let err = rt.execute(&compiled).unwrap_err();
    assert!(
        matches!(err, RuntimeError::Prepare { .. }),
        "unexpected: {err:?}"
    );
}

#[test]
fn reserved_params_fail_closed() {
    let mut rt = fixture();
    let mut compiled = compile("users.take(1) { name }");
    compiled.statements[0].params.push(Param::FromPrior {
        stmt: 0,
        column: "x".to_string(),
    });
    let err = rt.execute(&compiled).unwrap_err();
    assert!(
        matches!(err, RuntimeError::Contract(_)),
        "unexpected: {err:?}"
    );
    let mut compiled = compile("users.take(1) { name }");
    compiled.statements[0].params.push(Param::FromPriorKeys {
        stmt: 0,
        column: "x".to_string(),
    });
    let err = rt.execute(&compiled).unwrap_err();
    assert!(
        matches!(err, RuntimeError::Contract(_)),
        "unexpected: {err:?}"
    );
}

#[test]
fn instant_and_duration_columns_decode() {
    let mut rt = fixture();
    let compiled = compile("users.take(1) { created, ttl }");
    let outputs = rt.execute(&compiled).unwrap();
    let row = &outputs[0].rows[0];
    assert_eq!(
        row.get("created"),
        Some(&Value::Instant(
            DateTime::from_timestamp(1_700_000_000, 0).unwrap()
        ))
    );
    assert_eq!(
        row.get("ttl"),
        Some(&Value::Duration(TimeDelta::seconds(3600)))
    );
}
