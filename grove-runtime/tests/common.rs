
use chrono::{DateTime, TimeDelta};
use grove_query::codegen::model::CompiledQuery;
use grove_runtime::decode::{encode_dec, encode_duration, encode_instant};
use grove_runtime::{Runtime, Value};
use rust_decimal::Decimal;
use std::str::FromStr;

pub const SCHEMA_SRC: &str = r#"
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

pub fn compile(query: &str) -> CompiledQuery {
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

pub fn dec(s: &str) -> Decimal {
    Decimal::from_str(s).unwrap()
}

pub fn checked() -> Runtime {
    Runtime::open_in_memory().unwrap()
}

pub fn blob_param(bytes: &[u8; 13]) -> Vec<u8> {
    bytes.to_vec()
}

pub fn blob12(bytes: &[u8; 12]) -> Vec<u8> {
    bytes.to_vec()
}

pub fn fixture() -> Runtime {
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

pub fn names(rt: &mut Runtime, query: &str) -> Vec<String> {
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

pub fn query_dec(rt: &Runtime, sql: &str) -> Decimal {
    let bytes: Vec<u8> = rt.conn().query_row(sql, [], |row| row.get(0)).unwrap();
    grove_runtime::decode::decode_dec(&bytes, "c").unwrap()
}

pub fn query_int(rt: &Runtime, sql: &str) -> i64 {
    rt.conn().query_row(sql, [], |row| row.get(0)).unwrap()
}

pub fn query_text(rt: &Runtime, sql: &str) -> String {
    rt.conn().query_row(sql, [], |row| row.get(0)).unwrap()
}

pub fn instant_blob(secs: i64, nanos: u32) -> Vec<u8> {
    blob12(&encode_instant(
        &DateTime::from_timestamp(secs, nanos).unwrap(),
    ))
}

pub fn duration_blob(total_secs: i64) -> Vec<u8> {
    blob12(&encode_duration(&TimeDelta::seconds(total_secs)))
}

pub fn leap_instant() -> DateTime<chrono::Utc> {
    chrono::NaiveDate::from_ymd_opt(2024, 2, 29)
        .unwrap()
        .and_hms_opt(15, 4, 5)
        .unwrap()
        .and_utc()
}

pub fn agg_table(rt: &Runtime, values: &[Vec<u8>]) {
    rt.conn()
        .execute("CREATE TABLE IF NOT EXISTS m(v BLOB)", [])
        .unwrap();
    rt.conn().execute("DELETE FROM m", []).unwrap();
    for v in values {
        rt.conn()
            .execute("INSERT INTO m(v) VALUES (?)", [&v[..]])
            .unwrap();
    }
}

pub fn decs(strs: &[&str]) -> Vec<Vec<u8>> {
    strs.iter()
        .map(|s| blob_param(&encode_dec(&dec(s))))
        .collect()
}
