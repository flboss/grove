
#[allow(dead_code)]
#[path = "common.rs"]
mod common;

use chrono::{DateTime, TimeDelta};
use common::dec;
use grove_runtime::decode::{encode_dec, encode_duration, encode_instant};
use grove_runtime::{Runtime, Value};
use rust_decimal::Decimal;
use std::str::FromStr;

const SCHEMA_SRC: &str = r#"
    root users: User;
    root configs: Config;
    struct User {
        name: String,
        email: String@address,
        age: Int,
        score: Float,
        balance: Dec,
        active: Bool,
        nickname: ?String,
        pos: Tuple<Float, Float>@(lat, lng),
        trio: Tuple<Int, Tuple<Int, Int>>@(t0, t1, t2),
        created: Instant,
        ttl: Duration,
        tags: List<String>@id via user_tags[user_key, tag_value],
        scores: List<Int>@id via user_scores[user_key, score_value],
        durs: List<Duration>@id via user_durs[user_key, dur_value],
        whens: List<Instant>@id via user_whens[user_key, when_value],
        manager: &User,
        mentor: &?User,
        subordinates: &List<User>,
        mentees: &List<User>,
        orders: List<Order>,
        settings: Settings,
    }
    struct Order {
        total: Dec,
        user: &User,
        items: List<Item>,
    }
    struct Settings {
        theme: String,
        user: &User,
    }
    struct Item {
        sku: String,
        order: &Order,
    }
    struct Config {
        name: String,
        owner: String,
    }
    rel User.manager <<-> User.subordinates (users.manager_id -> users.id);
    rel User.mentor <<-> User.mentees (users.mentor_id -> users.id);
    rel Order.user <<-> User.orders (orders.user_id -> users.id);
    rel Settings.user <-> User.settings (settings.user_id -> users.id);
    rel Item.order <<-> Order.items (items.order_id -> orders.id);
"#;

fn compile_q(query: &str) -> grove_query::codegen::model::CompiledQuery {
    let schema = grove_schema::validate(
        grove_schema::parse_schema(SCHEMA_SRC)
            .0
            .expect("invalid test schema"),
    )
    .0
    .expect("invalid test schema");
    let (file, parse_diags) = grove_query::parse_query(query);
    assert!(parse_diags.is_empty(), "{query}");
    let (typed, type_diags) = grove_query::typecheck::typecheck(file.unwrap(), &schema);
    assert!(type_diags.is_empty(), "{query}");
    grove_query::codegen::codegen(&typed.unwrap(), &schema)
}

fn value(query: &str) -> Value {
    let mut rt = fixture();
    let compiled = compile_q(query);
    rt.execute_value(&compiled).unwrap()
}

fn t0() -> DateTime<chrono::Utc> {
    DateTime::from_timestamp(1_700_000_000, 0).unwrap()
}

fn t1() -> DateTime<chrono::Utc> {
    DateTime::from_timestamp(1_700_003_600, 0).unwrap()
}

fn fixture() -> Runtime {
    let rt = Runtime::open_in_memory().unwrap();
    rt.conn()
        .execute_batch(
            "CREATE TABLE users(id INTEGER, name TEXT, address TEXT, age INTEGER, score REAL,
                balance BLOB, active INTEGER, nickname TEXT, lat REAL, lng REAL,
                t0 INTEGER, t1 INTEGER, t2 INTEGER, created BLOB, ttl BLOB,
                manager_id INTEGER, mentor_id INTEGER);
             CREATE TABLE orders(id INTEGER, total BLOB, user_id INTEGER);
             CREATE TABLE settings(theme TEXT, user_id INTEGER);
             CREATE TABLE items(sku TEXT, order_id INTEGER);
             CREATE TABLE configs(name TEXT, owner TEXT);
             CREATE TABLE user_tags(user_key INTEGER, tag_value TEXT);
             CREATE TABLE user_scores(user_key INTEGER, score_value INTEGER);
             CREATE TABLE user_durs(user_key INTEGER, dur_value BLOB);
             CREATE TABLE user_whens(user_key INTEGER, when_value BLOB);",
        )
        .unwrap();
    let user = |id: i64,
                name: &str,
                email: &str,
                age: i64,
                score: f64,
                balance: &str,
                active: i64,
                nickname: Option<&str>,
                lat: f64,
                lng: f64,
                trio: (i64, i64, i64),
                created: DateTime<chrono::Utc>,
                ttl: i64,
                manager: i64,
                mentor: Option<i64>| {
        rt.conn()
            .execute(
                "INSERT INTO users(id, name, address, age, score, balance, active, nickname,
                    lat, lng, t0, t1, t2, created, ttl, manager_id, mentor_id)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                rusqlite::params![
                    id,
                    name,
                    email,
                    age,
                    score,
                    encode_dec(&dec(balance)).to_vec(),
                    active,
                    nickname,
                    lat,
                    lng,
                    trio.0,
                    trio.1,
                    trio.2,
                    encode_instant(&created).to_vec(),
                    encode_duration(&TimeDelta::seconds(ttl)).to_vec(),
                    manager,
                    mentor,
                ],
            )
            .unwrap();
    };
    user(
        1,
        "Ada",
        "a@x",
        30,
        1.5,
        "100.50",
        1,
        Some("Addy"),
        1.0,
        2.0,
        (1, 2, 3),
        t0(),
        3600,
        2,
        None,
    );
    user(
        2,
        "Bob",
        "b@x",
        40,
        2.5,
        "200.25",
        0,
        None,
        3.0,
        4.0,
        (4, 5, 6),
        t0(),
        7200,
        2,
        Some(1),
    );
    user(
        3,
        "Cyd",
        "c@x",
        50,
        3.5,
        "300.75",
        1,
        None,
        5.0,
        6.0,
        (7, 8, 9),
        t1(),
        1800,
        2,
        None,
    );
    let order = |id: i64, total: &str, user_id: i64| {
        rt.conn()
            .execute(
                "INSERT INTO orders(id, total, user_id) VALUES (?, ?, ?)",
                rusqlite::params![id, encode_dec(&dec(total)).to_vec(), user_id],
            )
            .unwrap();
    };
    order(10, "10.00", 1);
    order(11, "20.00", 1);
    order(12, "30.00", 2);
    rt.conn()
        .execute(
            "INSERT INTO settings(theme, user_id) VALUES ('dark', 1)",
            [],
        )
        .unwrap();
    for (sku, order_id) in [("a", 10), ("b", 10), ("c", 12)] {
        rt.conn()
            .execute(
                "INSERT INTO items(sku, order_id) VALUES (?, ?)",
                rusqlite::params![sku, order_id],
            )
            .unwrap();
    }
    for (name, owner) in [("n1", "a@x"), ("n2", "zzz")] {
        rt.conn()
            .execute(
                "INSERT INTO configs(name, owner) VALUES (?, ?)",
                rusqlite::params![name, owner],
            )
            .unwrap();
    }
    for (k, v) in [(1, "x"), (1, "y"), (2, "z")] {
        rt.conn()
            .execute(
                "INSERT INTO user_tags(user_key, tag_value) VALUES (?, ?)",
                rusqlite::params![k, v],
            )
            .unwrap();
    }
    for (k, v) in [(1, 5), (1, 7), (2, 9)] {
        rt.conn()
            .execute(
                "INSERT INTO user_scores(user_key, score_value) VALUES (?, ?)",
                rusqlite::params![k, v],
            )
            .unwrap();
    }
    for (k, v) in [(1, 60), (2, 120)] {
        rt.conn()
            .execute(
                "INSERT INTO user_durs(user_key, dur_value) VALUES (?, ?)",
                rusqlite::params![k, encode_duration(&TimeDelta::seconds(v)).to_vec()],
            )
            .unwrap();
    }
    for (k, v) in [(1, t0()), (2, t1())] {
        rt.conn()
            .execute(
                "INSERT INTO user_whens(user_key, when_value) VALUES (?, ?)",
                rusqlite::params![k, encode_instant(&v).to_vec()],
            )
            .unwrap();
    }
    rt
}

fn s(v: &str) -> Value {
    Value::String(v.to_string())
}
fn n(v: &str) -> Value {
    Value::Dec(Decimal::from_str(v).unwrap())
}
fn rec(fields: &[(&str, Value)]) -> Value {
    Value::Record(
        fields
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect(),
    )
}

fn multiset(v: &Value) -> Vec<String> {
    let Value::List(items) = v else {
        panic!("expected list, got {v:?}");
    };
    let mut out: Vec<String> = items.iter().map(|i| format!("{i:?}")).collect();
    out.sort();
    out
}


#[test]
fn slice_flat_projection() {
    assert_eq!(
        value("users { name, age }"),
        Value::List(vec![
            rec(&[("name", s("Ada")), ("age", Value::Int(30))]),
            rec(&[("name", s("Bob")), ("age", Value::Int(40))]),
            rec(&[("name", s("Cyd")), ("age", Value::Int(50))]),
        ])
    );
}

#[test]
fn slice_scalars() {
    assert_eq!(value("1 + 2"), Value::Int(3));
    assert_eq!(value("if true { 1 } else { 2 }"), Value::Int(1));
    assert_eq!(value("(1 as Float)"), Value::Float(1.0));
}


#[test]
fn slice_take_sort() {
    assert_eq!(
        value("users.sort_asc(age).take(2) { name }"),
        Value::List(vec![rec(&[("name", s("Ada"))]), rec(&[("name", s("Bob"))]),])
    );
    assert_eq!(
        value("users.sort_desc(age).skip(1).take(2) { name }"),
        Value::List(vec![rec(&[("name", s("Bob"))]), rec(&[("name", s("Ada"))]),])
    );
    assert_eq!(value("users.name.first()"), s("Ada"));
}


#[test]
fn slice_flatten() {
    let v = value("users.orders.flatten()");
    let totals: Vec<String> = multiset(&v)
        .iter()
        .map(|r| {
            assert!(r.contains("total"), "{r}");
            r.clone()
        })
        .collect();
    assert_eq!(totals.len(), 3);
    assert_eq!(value("[[1], [2]].flatten().sum()"), Value::Int(3));
}


#[test]
fn slice_optional_hops() {
    assert_eq!(
        value("users { mname = mentor?.name }"),
        Value::List(vec![
            rec(&[("mname", Value::Null)]),
            rec(&[("mname", s("Ada"))]),
            rec(&[("mname", Value::Null)]),
        ])
    );
    assert_eq!(
        value("users.manager.name"),
        Value::List(vec![rec(&[("name", s("Bob"))])])
    );
}


#[test]
fn slice_nested_children() {
    assert_eq!(
        value("users { name, o = orders { total } }"),
        Value::List(vec![
            rec(&[
                ("name", s("Ada")),
                (
                    "o",
                    Value::List(vec![
                        rec(&[("total", n("10")), ("user_id", Value::Int(1))]),
                        rec(&[("total", n("20")), ("user_id", Value::Int(1))]),
                    ])
                ),
            ]),
            rec(&[
                ("name", s("Bob")),
                (
                    "o",
                    Value::List(vec![rec(&[("total", n("30")), ("user_id", Value::Int(2))])]),
                ),
            ]),
            rec(&[("name", s("Cyd")), ("o", Value::List(vec![]))]),
        ])
    );
}


#[test]
fn slice_reductions() {
    assert_eq!(
        value("users { n = orders.len() }"),
        Value::List(vec![
            rec(&[("n", Value::Int(2))]),
            rec(&[("n", Value::Int(1))]),
            rec(&[("n", Value::Int(0))]),
        ])
    );
    assert_eq!(
        value("users { s = orders.sum_over(total) }"),
        Value::List(vec![
            rec(&[("s", n("30"))]),
            rec(&[("s", n("30"))]),
            rec(&[("s", Value::Null)]),
        ])
    );
    assert_eq!(value("[1, 2, 3].sum()"), Value::Int(6));
    assert_eq!(
        value("users { s = scores.sum() }"),
        Value::List(vec![
            rec(&[("s", Value::Int(12))]),
            rec(&[("s", Value::Int(9))]),
            rec(&[("s", Value::Null)]),
        ])
    );
    assert_eq!(
        value("users { a = scores.avg() }"),
        Value::List(vec![
            rec(&[("a", Value::Float(6.0))]),
            rec(&[("a", Value::Float(9.0))]),
            rec(&[("a", Value::Null)]),
        ])
    );
    assert_eq!(
        value("users { m = tags.max() }"),
        Value::List(vec![
            rec(&[("m", s("y"))]),
            rec(&[("m", s("z"))]),
            rec(&[("m", Value::Null)]),
        ])
    );
    assert_eq!(value("users.orders.len()"), Value::Int(3));
    assert_eq!(value("users.orders.flatten().len()"), Value::Int(3));
}


#[test]
fn slice_cross_root() {
    assert_eq!(
        value("users { name, cs = configs.filter(owner == email) }"),
        Value::List(vec![
            rec(&[
                ("name", s("Ada")),
                (
                    "cs",
                    Value::List(vec![rec(&[
                        ("name", s("n1")),
                        ("owner", s("a@x")),
                        ("$$pkey", s("a@x")),
                    ])]),
                ),
            ]),
            rec(&[("name", s("Bob")), ("cs", Value::List(vec![]))]),
            rec(&[("name", s("Cyd")), ("cs", Value::List(vec![]))]),
        ])
    );
    assert_eq!(
        value("users { t = configs.filter(owner == email).first()?.name }"),
        Value::List(vec![
            rec(&[("t", s("n1"))]),
            rec(&[("t", Value::Null)]),
            rec(&[("t", Value::Null)]),
        ])
    );
    assert_eq!(
        value("users { t = orders.filter(total > 15.0).first()?.total }"),
        Value::List(vec![
            rec(&[("t", n("20"))]),
            rec(&[("t", n("30"))]),
            rec(&[("t", Value::Null)]),
        ])
    );
}


#[test]
fn slice_literals() {
    assert_eq!(
        value("[1, 2, 3]"),
        Value::List(vec![
            rec(&[("c0", Value::Int(1))]),
            rec(&[("c0", Value::Int(2))]),
            rec(&[("c0", Value::Int(3))]),
        ])
    );
    assert_eq!(
        value("[\"a\", \"b\"]"),
        Value::List(vec![rec(&[("c0", s("a"))]), rec(&[("c0", s("b"))]),])
    );
    assert_eq!(
        value("[[1, 3], [2, 4]]"),
        Value::List(vec![
            rec(&[("c0", Value::Int(0)), ("c1", Value::Int(1))]),
            rec(&[("c0", Value::Int(0)), ("c1", Value::Int(3))]),
            rec(&[("c0", Value::Int(1)), ("c1", Value::Int(2))]),
            rec(&[("c0", Value::Int(1)), ("c1", Value::Int(4))]),
        ])
    );
}


#[test]
fn slice_tuples() {
    assert_eq!(
        value("users.filter(name == \"Ada\") { pos }"),
        Value::List(vec![rec(&[(
            "pos",
            Value::Tuple(vec![Value::Float(1.0), Value::Float(2.0)])
        )])])
    );
    assert_eq!(
        value("(1, 2.5)"),
        Value::Tuple(vec![Value::Int(1), n("2.5")])
    );
    assert_eq!(
        value("(1, [2, 3])"),
        Value::Tuple(vec![
            Value::Int(1),
            Value::List(vec![
                rec(&[("c0", Value::Int(2))]),
                rec(&[("c0", Value::Int(3))]),
            ]),
        ])
    );
    assert_eq!(
        value("{ a = [1, 2] }"),
        Value::Record(vec![(
            "a".to_string(),
            Value::List(vec![
                rec(&[("c0", Value::Int(1))]),
                rec(&[("c0", Value::Int(2))]),
            ]),
        )])
    );
}

#[test]
fn slice_structs() {
    assert_eq!(
        value("if true { { a = 1 } } else { { a = 2 } }"),
        Value::Record(vec![("a".to_string(), Value::Int(1))])
    );
    assert_eq!(
        value("users.filter(name == \"Ada\") { n = name, a = age }"),
        Value::List(vec![Value::Record(vec![
            ("n".to_string(), s("Ada")),
            ("a".to_string(), Value::Int(30)),
        ])])
    );
    assert_eq!(
        value("users[settings.theme == \"dark\"] { name }"),
        Value::List(vec![rec(&[("name", s("Ada"))])])
    );
}


#[test]
fn slice_unions() {
    assert_eq!(
        value("if true { [1] } else { [2] }"),
        Value::List(vec![rec(&[("c0", Value::Int(1))])])
    );
    assert_eq!(
        value("if true { (1, 2) } else { (3, 4) }"),
        Value::Tuple(vec![Value::Int(1), Value::Int(2)])
    );
    assert_eq!(
        value("users { x = if true { [1] } else { [2] } }"),
        Value::List(vec![
            rec(&[("x", Value::List(vec![rec(&[("c0", Value::Int(1))])]))]),
            rec(&[("x", Value::List(vec![rec(&[("c0", Value::Int(1))])]))]),
            rec(&[("x", Value::List(vec![rec(&[("c0", Value::Int(1))])]))]),
        ])
    );
}
