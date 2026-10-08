
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
    assert_eq!(value("users.manager.name"), Value::List(vec![s("Bob")]));
}

#[test]
fn slice_optional_list_temp_collapse() {
    assert_eq!(
        value("users { x = [[age]].first() }"),
        Value::List(vec![
            rec(&[("x", Value::List(vec![Value::Int(30)]))]),
            rec(&[("x", Value::List(vec![Value::Int(40)]))]),
            rec(&[("x", Value::List(vec![Value::Int(50)]))]),
        ])
    );
    assert_eq!(
        value("users { x = [[age]].nth(0) }"),
        Value::List(vec![
            rec(&[("x", Value::List(vec![Value::Int(30)]))]),
            rec(&[("x", Value::List(vec![Value::Int(40)]))]),
            rec(&[("x", Value::List(vec![Value::Int(50)]))]),
        ])
    );
    assert_eq!(
        value("users { x = [[age]].take(1).first() }"),
        Value::List(vec![
            rec(&[("x", Value::List(vec![Value::Int(30)]))]),
            rec(&[("x", Value::List(vec![Value::Int(40)]))]),
            rec(&[("x", Value::List(vec![Value::Int(50)]))]),
        ])
    );
    assert_eq!(
        value("users { x = [[age]].take(0).first() }"),
        Value::List(vec![
            rec(&[("x", Value::Null)]),
            rec(&[("x", Value::Null)]),
            rec(&[("x", Value::Null)]),
        ])
    );
    let twolists =
        |v: Value| Value::Tuple(vec![Value::List(vec![v.clone()]), Value::List(vec![v])]);
    assert_eq!(
        value("users { t = ([age], [[age]].first()) }"),
        Value::List(vec![
            rec(&[("t", twolists(Value::Int(30)))]),
            rec(&[("t", twolists(Value::Int(40)))]),
            rec(&[("t", twolists(Value::Int(50)))]),
        ])
    );
}

#[test]
fn slice_optional_list_single_chain() {
    let ord = |t: &str| Value::Record(vec![("total".to_string(), Value::Dec(dec(t)))]);
    assert_eq!(
        value("users { x = mentor?.orders }"),
        Value::List(vec![
            rec(&[("x", Value::Null)]),
            rec(&[("x", Value::List(vec![ord("10.00"), ord("20.00")]),)]),
            rec(&[("x", Value::Null)]),
        ])
    );
}

#[test]
fn slice_nested_collapse_chain() {
    let item = |sku: &str| rec(&[("sku", s(sku))]);
    let ada_pick = Value::List(vec![
        Value::List(vec![item("a"), item("b")]),
        Value::List(vec![]),
    ]);
    let bob_pick = Value::List(vec![Value::List(vec![item("c")])]);
    let cyd_pick = Value::List(vec![]);
    let Value::List(rows) = value("users { o = subordinates.orders.first()?.items }") else {
        panic!("root is a list");
    };
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0], rec(&[("o", Value::Null)]));
    assert_eq!(rows[2], rec(&[("o", Value::Null)]));
    let Value::Record(bob_fields) = &rows[1] else {
        panic!("row is a record");
    };
    assert_eq!(bob_fields.len(), 1);
    assert_eq!(bob_fields[0].0, "o");
    assert!(
        bob_fields[0].1 == ada_pick || bob_fields[0].1 == bob_pick || bob_fields[0].1 == cyd_pick,
        "unexpected Bob pick: {:?}",
        bob_fields[0].1
    );
}

#[test]
fn slice_nested_collapse_chain_value() {
    let item = |sku: &str| rec(&[("sku", s(sku))]);
    let ada_pick = Value::List(vec![
        Value::List(vec![item("a"), item("b")]),
        Value::List(vec![]),
    ]);
    let bob_pick = Value::List(vec![Value::List(vec![item("c")])]);
    let cyd_pick = Value::List(vec![]);
    let Value::List(rows) = value("users { o = subordinates.orders.first()?.items }") else {
        panic!("root is a list");
    };
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0], rec(&[("o", Value::Null)]));
    assert_eq!(rows[2], rec(&[("o", Value::Null)]));
    let Value::Record(bob_fields) = &rows[1] else {
        panic!("row is a record");
    };
    assert_eq!(bob_fields.len(), 1);
    assert_eq!(bob_fields[0].0, "o");
    assert!(
        bob_fields[0].1 == ada_pick || bob_fields[0].1 == bob_pick || bob_fields[0].1 == cyd_pick,
        "unexpected Bob pick: {:?}",
        bob_fields[0].1
    );
}

#[test]
fn slice_nested_scalar_temp_arms() {
    let t = |v: i64| rec(&[("t", Value::Int(v))]);
    assert_eq!(
        value("users { o = orders { t = [age].first() } }"),
        Value::List(vec![
            rec(&[("o", Value::List(vec![t(30), t(30)]))]),
            rec(&[("o", Value::List(vec![t(40)]))]),
            rec(&[("o", Value::List(vec![]))]),
        ])
    );
    let s = |v: &str| rec(&[("s", n(v))]);
    assert_eq!(
        value("users { o = orders.take(age) { s = [total].first() } }"),
        Value::List(vec![
            rec(&[("o", Value::List(vec![s("10"), s("20")]))]),
            rec(&[("o", Value::List(vec![s("30")]))]),
            rec(&[("o", Value::List(vec![]))]),
        ])
    );
}

#[test]
fn slice_nested_collapse_presence() {
    let b = |v: bool| rec(&[("o", Value::Bool(v))]);
    assert_eq!(
        value("users { o = subordinates.orders.first()?.items.is_some() }"),
        Value::List(vec![b(false), b(true), b(false)])
    );
    assert_eq!(
        value("users { o = subordinates.orders.first()?.items.is_none() }"),
        Value::List(vec![b(true), b(false), b(true)])
    );
    let h = |n: &str, v: bool| rec(&[("name", s(n)), ("h", Value::Bool(v))]);
    assert_eq!(
        value("users { name, h = users.orders.first().is_some() }"),
        Value::List(vec![h("Ada", true), h("Bob", true), h("Cyd", true),])
    );
}

#[test]
fn slice_nested_collection_temp_arms() {
    let it = |sku: &str, order_id: i64| rec(&[("sku", s(sku)), ("order_id", Value::Int(order_id))]);
    assert_eq!(
        value("users { o = orders { t = [items].first() } }"),
        Value::List(vec![
            rec(&[(
                "o",
                Value::List(vec![
                    rec(&[("t", Value::List(vec![it("a", 10), it("b", 10)]))]),
                    rec(&[("t", Value::List(vec![]))]),
                ]),
            )]),
            rec(&[(
                "o",
                Value::List(vec![rec(&[("t", Value::List(vec![it("c", 12)]))])]),
            )]),
            rec(&[("o", Value::List(vec![]))]),
        ])
    );
    assert_eq!(
        value("users { o = orders { t = [items].take(1).first() } }"),
        Value::List(vec![
            rec(&[(
                "o",
                Value::List(vec![
                    rec(&[("t", Value::List(vec![it("a", 10)]))]),
                    rec(&[("t", Value::List(vec![]))]),
                ]),
            )]),
            rec(&[(
                "o",
                Value::List(vec![rec(&[("t", Value::List(vec![it("c", 12)]))])]),
            )]),
            rec(&[("o", Value::List(vec![]))]),
        ])
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
                    Value::List(vec![rec(&[("total", n("10"))]), rec(&[("total", n("20"))]),])
                ),
            ]),
            rec(&[
                ("name", s("Bob")),
                ("o", Value::List(vec![rec(&[("total", n("30"))])]),),
            ]),
            rec(&[("name", s("Cyd")), ("o", Value::List(vec![]))]),
        ])
    );
}

#[test]
fn slice_via_array_values() {
    assert_eq!(
        value("users { sc = scores }"),
        Value::List(vec![
            rec(&[("sc", Value::List(vec![Value::Int(5), Value::Int(7)]))]),
            rec(&[("sc", Value::List(vec![Value::Int(9)]))]),
            rec(&[("sc", Value::List(vec![]))]),
        ])
    );
    assert_eq!(
        value("users { t = tags }"),
        Value::List(vec![
            rec(&[("t", Value::List(vec![s("x"), s("y")]))]),
            rec(&[("t", Value::List(vec![s("z")]))]),
            rec(&[("t", Value::List(vec![]))]),
        ])
    );
}

#[test]
fn cross_list_inner_values() {
    let ada = || rec(&[("s", Value::List(vec![Value::Int(5), Value::Int(7)]))]);
    let bob = || rec(&[("s", Value::List(vec![Value::Int(9)]))]);
    let cyd = || rec(&[("s", Value::List(vec![]))]);
    assert_eq!(
        value("users { x = configs { s = scores } }"),
        Value::List(vec![
            rec(&[("x", Value::List(vec![ada(), ada()]))]),
            rec(&[("x", Value::List(vec![bob(), bob()]))]),
            rec(&[("x", Value::List(vec![cyd(), cyd()]))]),
        ])
    );
}

#[test]
fn cross_list_inner_take_values() {
    assert_eq!(
        value("users { x = configs { s = scores.take(1) } }"),
        Value::List(vec![
            rec(&[(
                "x",
                Value::List(vec![
                    rec(&[("s", Value::List(vec![Value::Int(5)]))]),
                    rec(&[("s", Value::List(vec![Value::Int(5)]))]),
                ])
            )]),
            rec(&[(
                "x",
                Value::List(vec![
                    rec(&[("s", Value::List(vec![Value::Int(9)]))]),
                    rec(&[("s", Value::List(vec![Value::Int(9)]))]),
                ])
            )]),
            rec(&[(
                "x",
                Value::List(vec![
                    rec(&[("s", Value::List(vec![]))]),
                    rec(&[("s", Value::List(vec![]))]),
                ])
            )]),
        ])
    );
}

#[test]
fn cross_list_pipeline_values() {
    assert_eq!(
        value("users { x = configs.take(1) { s = scores } }"),
        Value::List(vec![
            rec(&[(
                "x",
                Value::List(vec![rec(&[(
                    "s",
                    Value::List(vec![Value::Int(5), Value::Int(7)])
                )])])
            )]),
            rec(&[(
                "x",
                Value::List(vec![rec(&[("s", Value::List(vec![Value::Int(9)]))])])
            )]),
            rec(&[("x", Value::List(vec![rec(&[("s", Value::List(vec![]))])]))]),
        ])
    );
}

#[test]
fn cross_list_mixed_values() {
    assert_eq!(
        value("users { x = configs { s = scores, n = name } }"),
        Value::List(vec![
            rec(&[(
                "x",
                Value::List(vec![
                    rec(&[
                        ("s", Value::List(vec![Value::Int(5), Value::Int(7)])),
                        ("n", s("n1")),
                    ]),
                    rec(&[
                        ("s", Value::List(vec![Value::Int(5), Value::Int(7)])),
                        ("n", s("n2")),
                    ]),
                ])
            )]),
            rec(&[(
                "x",
                Value::List(vec![
                    rec(&[("s", Value::List(vec![Value::Int(9)])), ("n", s("n1")),]),
                    rec(&[("s", Value::List(vec![Value::Int(9)])), ("n", s("n2")),]),
                ])
            )]),
            rec(&[(
                "x",
                Value::List(vec![
                    rec(&[("s", Value::List(vec![])), ("n", s("n1"))]),
                    rec(&[("s", Value::List(vec![])), ("n", s("n2"))]),
                ])
            )]),
        ])
    );
}

#[test]
fn cross_corr_filter_list_inner_values() {
    let ada = || rec(&[("s", Value::List(vec![Value::Int(5), Value::Int(7)]))]);
    assert_eq!(
        value("users { x = configs.filter(owner == email) { s = scores } }"),
        Value::List(vec![
            rec(&[("x", Value::List(vec![ada()]))]),
            rec(&[("x", Value::List(vec![]))]),
            rec(&[("x", Value::List(vec![]))]),
        ])
    );
    assert_eq!(
        value("users { x = configs.filter(owner == \"nobody\") { s = scores } }"),
        Value::List(vec![
            rec(&[("x", Value::List(vec![]))]),
            rec(&[("x", Value::List(vec![]))]),
            rec(&[("x", Value::List(vec![]))]),
        ])
    );
}

#[test]
fn cross_corr_filter_outer_scalar_values() {
    assert_eq!(
        value("users { x = configs.filter(owner == email) { s = age } }"),
        Value::List(vec![
            rec(&[("x", Value::List(vec![rec(&[("s", Value::Int(30))])]))]),
            rec(&[("x", Value::List(vec![]))]),
            rec(&[("x", Value::List(vec![]))]),
        ])
    );
}

#[test]
fn cross_corr_filter_nav_inner_values() {
    let ord = |t: &str| rec(&[("total", Value::Dec(dec(t)))]);
    assert_eq!(
        value("users { x = configs.filter(owner == email) { s = manager.orders } }"),
        Value::List(vec![
            rec(&[(
                "x",
                Value::List(vec![rec(&[("s", Value::List(vec![ord("30.00")]))])])
            )]),
            rec(&[("x", Value::List(vec![]))]),
            rec(&[("x", Value::List(vec![]))]),
        ])
    );
}

#[test]
fn nested_take_bound_middle_values() {
    let item = |sku: &str| rec(&[("sku", s(sku))]);
    assert_eq!(
        value("users { o = orders.take(age) { s = items } }"),
        Value::List(vec![
            rec(&[(
                "o",
                Value::List(vec![
                    rec(&[("s", Value::List(vec![item("a"), item("b")]))]),
                    rec(&[("s", Value::List(vec![]))]),
                ])
            )]),
            rec(&[(
                "o",
                Value::List(vec![rec(&[("s", Value::List(vec![item("c")]))])])
            )]),
            rec(&[("o", Value::List(vec![]))]),
        ])
    );
}

#[test]
fn nested_take_bound_inner_values() {
    let item = |sku: &str| rec(&[("sku", s(sku))]);
    assert_eq!(
        value("users { o = orders { s = items.take(age) } }"),
        Value::List(vec![
            rec(&[(
                "o",
                Value::List(vec![
                    rec(&[("s", Value::List(vec![item("a"), item("b")]))]),
                    rec(&[("s", Value::List(vec![]))]),
                ])
            )]),
            rec(&[(
                "o",
                Value::List(vec![rec(&[("s", Value::List(vec![item("c")]))])])
            )]),
            rec(&[("o", Value::List(vec![]))]),
        ])
    );
}

#[test]
fn chained_projection_reuses_values() {
    let ord = |t: &str| Value::Record(vec![("total".to_string(), Value::Dec(dec(t)))]);
    assert_eq!(
        value("users { x = orders } { y = x }"),
        Value::List(vec![
            rec(&[("y", Value::List(vec![ord("10"), ord("20")]))]),
            rec(&[("y", Value::List(vec![ord("30")]))]),
            rec(&[("y", Value::List(vec![]))]),
        ])
    );
}

#[test]
fn chained_projection_take_values() {
    let ord = |t: &str| Value::Record(vec![("total".to_string(), Value::Dec(dec(t)))]);
    assert_eq!(
        value("users { x = orders } { y = x.take(1) }"),
        Value::List(vec![
            rec(&[("y", Value::List(vec![ord("10")]))]),
            rec(&[("y", Value::List(vec![ord("30")]))]),
            rec(&[("y", Value::List(vec![]))]),
        ])
    );
}

#[test]
fn projected_list_agg_values() {
    assert_eq!(
        value("users { x = scores } { y = x.sum() }"),
        Value::List(vec![
            rec(&[("y", Value::Int(12))]),
            rec(&[("y", Value::Int(9))]),
            rec(&[("y", Value::Null)]),
        ])
    );
}

#[test]
fn projected_records_nav_values() {
    assert_eq!(
        value("users { m = manager } { x = m.name }"),
        Value::List(vec![
            rec(&[("x", s("Bob"))]),
            rec(&[("x", s("Bob"))]),
            rec(&[("x", s("Bob"))]),
        ])
    );
}

#[test]
fn projected_nonscalar_field_values() {
    let ord = |t: &str| Value::Record(vec![("total".to_string(), Value::Dec(dec(t)))]);
    assert_eq!(
        value("users { o = orders }.o"),
        Value::List(vec![
            rec(&[("o", Value::List(vec![ord("10"), ord("20")]))]),
            rec(&[("o", Value::List(vec![ord("30")]))]),
            rec(&[("o", Value::List(vec![]))]),
        ])
    );
}

#[test]
fn slice_nested_via_root() {
    assert_eq!(
        value("users.scores"),
        Value::List(vec![
            Value::List(vec![Value::Int(5), Value::Int(7)]),
            Value::List(vec![Value::Int(9)]),
            Value::List(vec![]),
        ])
    );
    assert_eq!(
        value("users.scores.take(2)"),
        Value::List(vec![
            Value::List(vec![Value::Int(5), Value::Int(7)]),
            Value::List(vec![Value::Int(9)]),
        ])
    );
    assert_eq!(
        value("users.scores.flatten()"),
        Value::List(vec![Value::Int(5), Value::Int(7), Value::Int(9)])
    );
}

#[test]
fn slice_take_window_values() {
    assert_eq!(
        value("users { o = orders.take(2) { total } }"),
        Value::List(vec![
            rec(&[(
                "o",
                Value::List(vec![rec(&[("total", n("10"))]), rec(&[("total", n("20"))])]),
            )]),
            rec(&[("o", Value::List(vec![rec(&[("total", n("30"))])]))]),
            rec(&[("o", Value::List(vec![]))]),
        ])
    );
}

#[test]
fn slice_broadcast_nested() {
    assert_eq!(
        value("users { x = [[1, 2], [3]] }"),
        Value::List(vec![
            rec(&[(
                "x",
                Value::List(vec![
                    Value::List(vec![Value::Int(1), Value::Int(2)]),
                    Value::List(vec![Value::Int(3)]),
                ]),
            )]),
            rec(&[(
                "x",
                Value::List(vec![
                    Value::List(vec![Value::Int(1), Value::Int(2)]),
                    Value::List(vec![Value::Int(3)]),
                ]),
            )]),
            rec(&[(
                "x",
                Value::List(vec![
                    Value::List(vec![Value::Int(1), Value::Int(2)]),
                    Value::List(vec![Value::Int(3)]),
                ]),
            )]),
        ])
    );
}

#[test]
fn slice_declared_key_column() {
    const SCHEMA: &str = r#"
        root users: User;
        struct User {
            name: String,
            orders: List<Order>,
        }
        struct Order {
            user_id: Int,
            total: Dec,
            user: &User,
        }
        rel Order.user <<-> User.orders (orders.user_id -> users.id);
    "#;
    let schema = grove_schema::validate(grove_schema::parse_schema(SCHEMA).0.unwrap())
        .0
        .unwrap();
    let compile = |query: &str| {
        let (file, parse_diags) = grove_query::parse_query(query);
        assert!(parse_diags.is_empty());
        let (typed, type_diags) = grove_query::typecheck::typecheck(file.unwrap(), &schema);
        assert!(type_diags.is_empty());
        grove_query::codegen::codegen(&typed.unwrap(), &schema)
    };
    let mut rt = Runtime::open_in_memory().unwrap();
    let zero = encode_dec(&Decimal::ZERO);
    rt.conn()
        .execute_batch("CREATE TABLE users(id INTEGER, name TEXT);")
        .unwrap();
    rt.conn()
        .execute(
            "CREATE TABLE orders(id INTEGER, user_id INTEGER, total BLOB);",
            [],
        )
        .unwrap();
    rt.conn()
        .execute("INSERT INTO users(id, name) VALUES (1, 'Ada')", [])
        .unwrap();
    rt.conn()
        .execute(
            "INSERT INTO orders(id, user_id, total) VALUES (10, 1, ?)",
            rusqlite::params![zero.to_vec()],
        )
        .unwrap();
    assert_eq!(
        rt.execute_value(&compile("users { o = orders }")).unwrap(),
        Value::List(vec![Value::Record(vec![(
            "o".to_string(),
            Value::List(vec![Value::Record(vec![
                ("user_id".to_string(), Value::Int(1)),
                ("total".to_string(), Value::Dec(Decimal::ZERO)),
            ])]),
        )])])
    );
    assert_eq!(
        rt.execute_value(&compile("users { o = orders { total } }"))
            .unwrap(),
        Value::List(vec![Value::Record(vec![(
            "o".to_string(),
            Value::List(vec![Value::Record(vec![(
                "total".to_string(),
                Value::Dec(Decimal::ZERO)
            )])]),
        )])])
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
                    Value::List(vec![rec(&[("name", s("n1")), ("owner", s("a@x")),])]),
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
    assert_eq!(
        value("users { name, cs = configs.filter((owner, owner) == (email, email)) { name } }"),
        Value::List(vec![
            rec(&[
                ("name", s("Ada")),
                ("cs", Value::List(vec![rec(&[("name", s("n1"))])])),
            ]),
            rec(&[("name", s("Bob")), ("cs", Value::List(vec![]))]),
            rec(&[("name", s("Cyd")), ("cs", Value::List(vec![]))]),
        ])
    );
    assert_eq!(
        value(r#"users { name, cs = configs.filter((owner, name) == (email, "n1")) { name } }"#),
        Value::List(vec![
            rec(&[
                ("name", s("Ada")),
                ("cs", Value::List(vec![rec(&[("name", s("n1"))])])),
            ]),
            rec(&[("name", s("Bob")), ("cs", Value::List(vec![]))]),
            rec(&[("name", s("Cyd")), ("cs", Value::List(vec![]))]),
        ])
    );
    assert_eq!(
        value("users { name, cs = configs.filter(owner == email && name == email) { name } }"),
        Value::List(vec![
            rec(&[("name", s("Ada")), ("cs", Value::List(vec![]))]),
            rec(&[("name", s("Bob")), ("cs", Value::List(vec![]))]),
            rec(&[("name", s("Cyd")), ("cs", Value::List(vec![]))]),
        ])
    );
    assert_eq!(
        value("users { name, cs = configs.filter((owner, name) == (email, email)) { name } }"),
        Value::List(vec![
            rec(&[("name", s("Ada")), ("cs", Value::List(vec![]))]),
            rec(&[("name", s("Bob")), ("cs", Value::List(vec![]))]),
            rec(&[("name", s("Cyd")), ("cs", Value::List(vec![]))]),
        ])
    );
    assert_eq!(
        value(
            r#"users { name, cs = configs.filter(manager.email == owner || manager.email == "x") { name } }"#
        ),
        Value::List(vec![
            rec(&[("name", s("Ada")), ("cs", Value::List(vec![]))]),
            rec(&[("name", s("Bob")), ("cs", Value::List(vec![]))]),
            rec(&[("name", s("Cyd")), ("cs", Value::List(vec![]))]),
        ])
    );
    assert_eq!(
        value("users { name, cs = configs.filter(owner == manager.manager.email) { name } }"),
        Value::List(vec![
            rec(&[("name", s("Ada")), ("cs", Value::List(vec![]))]),
            rec(&[("name", s("Bob")), ("cs", Value::List(vec![]))]),
            rec(&[("name", s("Cyd")), ("cs", Value::List(vec![]))]),
        ])
    );
}


#[test]
fn slice_literals() {
    assert_eq!(
        value("[1, 2, 3]"),
        Value::List(vec![Value::Int(1), Value::Int(2), Value::Int(3),])
    );
    assert_eq!(value("[\"a\", \"b\"]"), Value::List(vec![s("a"), s("b"),]));
    assert_eq!(
        value("[[1, 3], [2, 4]]"),
        Value::List(vec![
            Value::List(vec![Value::Int(1), Value::Int(3)]),
            Value::List(vec![Value::Int(2), Value::Int(4)]),
        ])
    );
    assert_eq!(
        value("[[[1]]]"),
        Value::List(vec![Value::List(vec![Value::List(vec![Value::Int(1)])])])
    );
    assert_eq!(
        value("[(1, 2), (3, 4)]"),
        Value::List(vec![
            Value::Tuple(vec![Value::Int(1), Value::Int(2)]),
            Value::Tuple(vec![Value::Int(3), Value::Int(4)]),
        ])
    );
    assert_eq!(
        value("[[(1, 2)], [(3, 4)]]"),
        Value::List(vec![
            Value::List(vec![Value::Tuple(vec![Value::Int(1), Value::Int(2)])]),
            Value::List(vec![Value::Tuple(vec![Value::Int(3), Value::Int(4)])]),
        ])
    );
    assert_eq!(
        value("[[(20, [1, 2])], [(30, [3, 4, 5])]]"),
        Value::List(vec![
            Value::List(vec![Value::Tuple(vec![
                Value::Int(20),
                Value::List(vec![Value::Int(1), Value::Int(2)]),
            ])]),
            Value::List(vec![Value::Tuple(vec![
                Value::Int(30),
                Value::List(vec![Value::Int(3), Value::Int(4), Value::Int(5)]),
            ])]),
        ])
    );
    assert_eq!(
        value("[(1, [2])]"),
        Value::List(vec![Value::Tuple(vec![
            Value::Int(1),
            Value::List(vec![Value::Int(2)]),
        ])])
    );
}

#[test]
fn slice_record_literals() {
    assert_eq!(
        value("[{name = \"Bob\", age = 40}, {name = \"Alice\", age = 30}]"),
        Value::List(vec![
            rec(&[("name", s("Bob")), ("age", Value::Int(40))]),
            rec(&[("name", s("Alice")), ("age", Value::Int(30))]),
        ])
    );
    assert_eq!(
        value("[{a = 1, b = 2}, {b = 3, a = 4}]"),
        Value::List(vec![
            rec(&[("a", Value::Int(1)), ("b", Value::Int(2))]),
            rec(&[("a", Value::Int(4)), ("b", Value::Int(3))]),
        ])
    );
    assert_eq!(
        value("[[{a = 1}]]"),
        Value::List(vec![Value::List(vec![rec(&[("a", Value::Int(1))])])])
    );
    assert_eq!(
        value("[{a = {x = 1}}]"),
        Value::List(vec![rec(&[(
            "a",
            Value::Record(vec![("x".to_string(), Value::Int(1))])
        )])])
    );
    assert_eq!(
        value("[[{a = [1, 2]}]]"),
        Value::List(vec![Value::List(vec![rec(&[(
            "a",
            Value::List(vec![Value::Int(1), Value::Int(2)])
        )])])])
    );
    assert_eq!(
        value("[(1, {x = 1})]"),
        Value::List(vec![Value::Tuple(vec![
            Value::Int(1),
            Value::Record(vec![("x".to_string(), Value::Int(1))]),
        ])])
    );
    assert_eq!(
        value("users { x = [{a = 1}] }"),
        Value::List(vec![
            rec(&[("x", Value::List(vec![rec(&[("a", Value::Int(1))])]))]),
            rec(&[("x", Value::List(vec![rec(&[("a", Value::Int(1))])]))]),
            rec(&[("x", Value::List(vec![rec(&[("a", Value::Int(1))])]))]),
        ])
    );
}

#[test]
fn slice_computed_literals() {
    assert_eq!(
        value("[1 + 2, 3]"),
        Value::List(vec![Value::Int(3), Value::Int(3)])
    );
    assert_eq!(value("[\"abc\".len()]"), Value::List(vec![Value::Int(3)]));
    assert_eq!(
        value("[[1 + 2]]"),
        Value::List(vec![Value::List(vec![Value::Int(3)])])
    );
    assert_eq!(
        value("[(20, [1 + 1])]"),
        Value::List(vec![Value::Tuple(vec![
            Value::Int(20),
            Value::List(vec![Value::Int(2)]),
        ])])
    );
    assert_eq!(
        value("[{a = 1 + 2}]"),
        Value::List(vec![rec(&[("a", Value::Int(3))])])
    );
    assert_eq!(
        value("[(1, {x = 1 + 2})]"),
        Value::List(vec![Value::Tuple(vec![
            Value::Int(1),
            Value::Record(vec![("x".to_string(), Value::Int(3))]),
        ])])
    );
    assert_eq!(
        value("[if true {1} else {2}]"),
        Value::List(vec![Value::Int(1)])
    );
    assert_eq!(
        value("[if false {1} else {2}]"),
        Value::List(vec![Value::Int(2)])
    );
    assert_eq!(value("[(users.name.first())]"), Value::List(vec![s("Ada")]));
    assert_eq!(value("[1 + 2, 3].sum()"), Value::Int(6));
    assert_eq!(value("[1 + 1, 2].contains(2)"), Value::Bool(true));
    assert_eq!(
        value("[[1 + 2], [3]].flatten()"),
        Value::List(vec![Value::Int(3), Value::Int(3)])
    );
    assert_eq!(
        value("[1 + 2, 3].take(1)"),
        Value::List(vec![Value::Int(3)])
    );
    assert_eq!(
        value("users { x = [1 + 2] }"),
        Value::List(vec![
            rec(&[("x", Value::List(vec![Value::Int(3)]))]),
            rec(&[("x", Value::List(vec![Value::Int(3)]))]),
            rec(&[("x", Value::List(vec![Value::Int(3)]))]),
        ])
    );
    assert_eq!(
        value("if true { [1 + 2] } else { [3] }"),
        Value::List(vec![Value::Int(3)])
    );
    assert!(matches!(
        value("[@now]"),
        Value::List(v) if v.len() == 1 && matches!(&v[0], Value::Instant(_))
    ));
}

#[test]
fn slice_computed_neg_overflow_errors() {
    let mut rt = fixture();
    let compiled = compile_q("[-Int::MIN]");
    let err = rt.execute_value(&compiled).unwrap_err();
    assert!(
        matches!(err, grove_runtime::RuntimeError::Step { .. }),
        "expected execution failure, got {err:?}"
    );
}

#[test]
fn slice_correlated_arrays() {
    assert_eq!(
        value("users { x = [age] }"),
        Value::List(vec![
            rec(&[("x", Value::List(vec![Value::Int(30)]))]),
            rec(&[("x", Value::List(vec![Value::Int(40)]))]),
            rec(&[("x", Value::List(vec![Value::Int(50)]))]),
        ])
    );
    assert_eq!(
        value("users { x = [age, age + 1] }"),
        Value::List(vec![
            rec(&[("x", Value::List(vec![Value::Int(30), Value::Int(31)]))]),
            rec(&[("x", Value::List(vec![Value::Int(40), Value::Int(41)]))]),
            rec(&[("x", Value::List(vec![Value::Int(50), Value::Int(51)]))]),
        ])
    );
    assert_eq!(
        value("users { x = [(age, 1)] }"),
        Value::List(vec![
            rec(&[(
                "x",
                Value::List(vec![Value::Tuple(vec![Value::Int(30), Value::Int(1)])]),
            )]),
            rec(&[(
                "x",
                Value::List(vec![Value::Tuple(vec![Value::Int(40), Value::Int(1)])]),
            )]),
            rec(&[(
                "x",
                Value::List(vec![Value::Tuple(vec![Value::Int(50), Value::Int(1)])]),
            )]),
        ])
    );
    assert_eq!(
        value("users { x = [{a = age}] }"),
        Value::List(vec![
            rec(&[("x", Value::List(vec![rec(&[("a", Value::Int(30))])]))]),
            rec(&[("x", Value::List(vec![rec(&[("a", Value::Int(40))])]))]),
            rec(&[("x", Value::List(vec![rec(&[("a", Value::Int(50))])]))]),
        ])
    );
    assert_eq!(
        value("users { x = [if age > 40 {1} else {2}] }"),
        Value::List(vec![
            rec(&[("x", Value::List(vec![Value::Int(2)]))]),
            rec(&[("x", Value::List(vec![Value::Int(2)]))]),
            rec(&[("x", Value::List(vec![Value::Int(1)]))]),
        ])
    );
    assert_eq!(
        value("users { x = [{a = age, b = name}] }"),
        Value::List(vec![
            rec(&[(
                "x",
                Value::List(vec![rec(&[("a", Value::Int(30)), ("b", s("Ada"))])]),
            )]),
            rec(&[(
                "x",
                Value::List(vec![rec(&[("a", Value::Int(40)), ("b", s("Bob"))])]),
            )]),
            rec(&[(
                "x",
                Value::List(vec![rec(&[("a", Value::Int(50)), ("b", s("Cyd"))])]),
            )]),
        ])
    );
    assert_eq!(
        value("users { n = name } { x = [n] }"),
        Value::List(vec![
            rec(&[("x", Value::List(vec![s("Ada")]))]),
            rec(&[("x", Value::List(vec![s("Bob")]))]),
            rec(&[("x", Value::List(vec![s("Cyd")]))]),
        ])
    );
    assert_eq!(
        value("users { x = [age], n = name }"),
        Value::List(vec![
            rec(&[("x", Value::List(vec![Value::Int(30)])), ("n", s("Ada"))]),
            rec(&[("x", Value::List(vec![Value::Int(40)])), ("n", s("Bob"))]),
            rec(&[("x", Value::List(vec![Value::Int(50)])), ("n", s("Cyd"))]),
        ])
    );
}

#[test]
fn slice_nested_correlated_arrays() {
    assert_eq!(
        value("users { x = [[age]] }"),
        Value::List(vec![
            rec(&[("x", Value::List(vec![Value::List(vec![Value::Int(30)])]))]),
            rec(&[("x", Value::List(vec![Value::List(vec![Value::Int(40)])]))]),
            rec(&[("x", Value::List(vec![Value::List(vec![Value::Int(50)])]))]),
        ])
    );
    assert_eq!(
        value("users { x = [[age], [1]] }"),
        Value::List(vec![
            rec(&[(
                "x",
                Value::List(vec![
                    Value::List(vec![Value::Int(30)]),
                    Value::List(vec![Value::Int(1)]),
                ]),
            )]),
            rec(&[(
                "x",
                Value::List(vec![
                    Value::List(vec![Value::Int(40)]),
                    Value::List(vec![Value::Int(1)]),
                ]),
            )]),
            rec(&[(
                "x",
                Value::List(vec![
                    Value::List(vec![Value::Int(50)]),
                    Value::List(vec![Value::Int(1)]),
                ]),
            )]),
        ])
    );
    assert_eq!(
        value("users { x = [[[age]]] }"),
        Value::List(vec![
            rec(&[(
                "x",
                Value::List(vec![Value::List(vec![Value::List(vec![Value::Int(30)])])]),
            )]),
            rec(&[(
                "x",
                Value::List(vec![Value::List(vec![Value::List(vec![Value::Int(40)])])]),
            )]),
            rec(&[(
                "x",
                Value::List(vec![Value::List(vec![Value::List(vec![Value::Int(50)])])]),
            )]),
        ])
    );
    assert_eq!(
        value("users { x = [(age, [1])] }"),
        Value::List(vec![
            rec(&[(
                "x",
                Value::List(vec![Value::Tuple(vec![
                    Value::Int(30),
                    Value::List(vec![Value::Int(1)]),
                ])]),
            )]),
            rec(&[(
                "x",
                Value::List(vec![Value::Tuple(vec![
                    Value::Int(40),
                    Value::List(vec![Value::Int(1)]),
                ])]),
            )]),
            rec(&[(
                "x",
                Value::List(vec![Value::Tuple(vec![
                    Value::Int(50),
                    Value::List(vec![Value::Int(1)]),
                ])]),
            )]),
        ])
    );
    assert_eq!(
        value("users { x = [{a = age, b = [1]}] }"),
        Value::List(vec![
            rec(&[(
                "x",
                Value::List(vec![rec(&[
                    ("a", Value::Int(30)),
                    ("b", Value::List(vec![Value::Int(1)])),
                ])]),
            )]),
            rec(&[(
                "x",
                Value::List(vec![rec(&[
                    ("a", Value::Int(40)),
                    ("b", Value::List(vec![Value::Int(1)])),
                ])]),
            )]),
            rec(&[(
                "x",
                Value::List(vec![rec(&[
                    ("a", Value::Int(50)),
                    ("b", Value::List(vec![Value::Int(1)])),
                ])]),
            )]),
        ])
    );
    assert_eq!(
        value("users { x = [age, age + 1].nth(age - 30) }"),
        Value::List(vec![
            rec(&[("x", Value::Int(30))]),
            rec(&[("x", Value::Null)]),
            rec(&[("x", Value::Null)]),
        ])
    );
    assert_eq!(
        value("users { x = [(users.first()?.name)] }"),
        Value::List(vec![
            rec(&[("x", Value::List(vec![s("Ada")]))]),
            rec(&[("x", Value::List(vec![s("Ada")]))]),
            rec(&[("x", Value::List(vec![s("Ada")]))]),
        ])
    );
    assert_eq!(
        value("users { x = [{a = age}, {a = age + 1}].nth(1)?.a }"),
        Value::List(vec![
            rec(&[("x", Value::Int(31))]),
            rec(&[("x", Value::Int(41))]),
            rec(&[("x", Value::Int(51))]),
        ])
    );
    assert_eq!(
        value("users { x = [age, age + 1].nth(age - 30) }"),
        Value::List(vec![
            rec(&[("x", Value::Int(30))]),
            rec(&[("x", Value::Null)]),
            rec(&[("x", Value::Null)]),
        ])
    );
    assert_eq!(
        value("users { x = [{a = 1}, {a = 2}].nth(1)?.a }"),
        Value::List(vec![
            rec(&[("x", Value::Int(2))]),
            rec(&[("x", Value::Int(2))]),
            rec(&[("x", Value::Int(2))]),
        ])
    );
    assert_eq!(
        value("users { x = [{a = 1}, {a = 2}].nth(5)?.a }"),
        Value::List(vec![
            rec(&[("x", Value::Null)]),
            rec(&[("x", Value::Null)]),
            rec(&[("x", Value::Null)]),
        ])
    );
}

#[test]
fn slice_temp_method_tails() {
    assert_eq!(
        value("users { x = [age, age + 1].take(1) }"),
        Value::List(vec![
            rec(&[("x", Value::List(vec![Value::Int(30)]))]),
            rec(&[("x", Value::List(vec![Value::Int(40)]))]),
            rec(&[("x", Value::List(vec![Value::Int(50)]))]),
        ])
    );
    assert_eq!(
        value("users { x = [age, age + 1].skip(1) }"),
        Value::List(vec![
            rec(&[("x", Value::List(vec![Value::Int(31)]))]),
            rec(&[("x", Value::List(vec![Value::Int(41)]))]),
            rec(&[("x", Value::List(vec![Value::Int(51)]))]),
        ])
    );
    assert_eq!(
        value("users { x = [age, age + 1].sort_desc(age) }"),
        Value::List(vec![
            rec(&[("x", Value::List(vec![Value::Int(31), Value::Int(30)]))]),
            rec(&[("x", Value::List(vec![Value::Int(41), Value::Int(40)]))]),
            rec(&[("x", Value::List(vec![Value::Int(51), Value::Int(50)]))]),
        ])
    );
    assert_eq!(
        value("users { x = [{a = age}].filter(true) }"),
        Value::List(vec![
            rec(&[("x", Value::List(vec![rec(&[("a", Value::Int(30))])]))]),
            rec(&[("x", Value::List(vec![rec(&[("a", Value::Int(40))])]))]),
            rec(&[("x", Value::List(vec![rec(&[("a", Value::Int(50))])]))]),
        ])
    );
    assert_eq!(
        value("users { x = [[age], [age + 1]].flatten() }"),
        Value::List(vec![
            rec(&[("x", Value::List(vec![Value::Int(30), Value::Int(31)]))]),
            rec(&[("x", Value::List(vec![Value::Int(40), Value::Int(41)]))]),
            rec(&[("x", Value::List(vec![Value::Int(50), Value::Int(51)]))]),
        ])
    );
    assert_eq!(
        value("users { x = [[age], [age + 1]].take(1) }"),
        Value::List(vec![
            rec(&[("x", Value::List(vec![Value::List(vec![Value::Int(30)])]))]),
            rec(&[("x", Value::List(vec![Value::List(vec![Value::Int(40)])]))]),
            rec(&[("x", Value::List(vec![Value::List(vec![Value::Int(50)])]))]),
        ])
    );
    assert_eq!(
        value("users { x = [1, 2, 3].take(age - 30) }"),
        Value::List(vec![
            rec(&[("x", Value::List(vec![]))]),
            rec(&[(
                "x",
                Value::List(vec![Value::Int(1), Value::Int(2), Value::Int(3)])
            )]),
            rec(&[(
                "x",
                Value::List(vec![Value::Int(1), Value::Int(2), Value::Int(3)])
            )]),
        ])
    );
    assert_eq!(
        value("users { x = [age, age + 1].take(age - 30) }"),
        Value::List(vec![
            rec(&[("x", Value::List(vec![]))]),
            rec(&[("x", Value::List(vec![Value::Int(40), Value::Int(41)]),)]),
            rec(&[("x", Value::List(vec![Value::Int(50), Value::Int(51)]),)]),
        ])
    );
    assert_eq!(
        value("users { x = [age, age + 1].take(1).sort_asc(age) }"),
        Value::List(vec![
            rec(&[("x", Value::List(vec![Value::Int(30)]))]),
            rec(&[("x", Value::List(vec![Value::Int(40)]))]),
            rec(&[("x", Value::List(vec![Value::Int(50)]))]),
        ])
    );
    assert_eq!(
        value("users { x = [{a = 3}, {a = 1}, {a = 2}].take(2).sort_asc(a) }"),
        Value::List(vec![
            rec(&[(
                "x",
                Value::List(vec![
                    rec(&[("a", Value::Int(1))]),
                    rec(&[("a", Value::Int(3))])
                ]),
            )]),
            rec(&[(
                "x",
                Value::List(vec![
                    rec(&[("a", Value::Int(1))]),
                    rec(&[("a", Value::Int(3))])
                ]),
            )]),
            rec(&[(
                "x",
                Value::List(vec![
                    rec(&[("a", Value::Int(1))]),
                    rec(&[("a", Value::Int(3))])
                ]),
            )]),
        ])
    );
}

#[test]
fn slice_scalar_temp_arrays() {
    assert_eq!(
        value("users { x = [age, age + 1].sum() }"),
        Value::List(vec![
            rec(&[("x", Value::Int(61))]),
            rec(&[("x", Value::Int(81))]),
            rec(&[("x", Value::Int(101))]),
        ])
    );
    assert_eq!(
        value("users { x = [age, age + 1].contains(30) }"),
        Value::List(vec![
            rec(&[("x", Value::Bool(true))]),
            rec(&[("x", Value::Bool(false))]),
            rec(&[("x", Value::Bool(false))]),
        ])
    );
    assert_eq!(
        value("users { x = [age, age + 1].first() }"),
        Value::List(vec![
            rec(&[("x", Value::Int(30))]),
            rec(&[("x", Value::Int(40))]),
            rec(&[("x", Value::Int(50))]),
        ])
    );
    assert_eq!(
        value("users { x = [age, age + 1].nth(1) }"),
        Value::List(vec![
            rec(&[("x", Value::Int(31))]),
            rec(&[("x", Value::Int(41))]),
            rec(&[("x", Value::Int(51))]),
        ])
    );
    assert_eq!(
        value("users { x = [age, age + 1].len() }"),
        Value::List(vec![
            rec(&[("x", Value::Int(2))]),
            rec(&[("x", Value::Int(2))]),
            rec(&[("x", Value::Int(2))]),
        ])
    );
    assert_eq!(
        value("users { x = [age, age + 1].avg() }"),
        Value::List(vec![
            rec(&[("x", Value::Float(30.5))]),
            rec(&[("x", Value::Float(40.5))]),
            rec(&[("x", Value::Float(50.5))]),
        ])
    );
    assert_eq!(
        value("users { x = [age, age + 1].max() }"),
        Value::List(vec![
            rec(&[("x", Value::Int(31))]),
            rec(&[("x", Value::Int(41))]),
            rec(&[("x", Value::Int(51))]),
        ])
    );
}

#[test]
fn slice_navigated_arrays() {
    assert_eq!(
        value("users { x = [manager.name] }"),
        Value::List(vec![
            rec(&[("x", Value::List(vec![s("Bob")]))]),
            rec(&[("x", Value::List(vec![s("Bob")]))]),
            rec(&[("x", Value::List(vec![s("Bob")]))]),
        ])
    );
    assert_eq!(
        value("users { x = [[manager.name]] }"),
        Value::List(vec![
            rec(&[("x", Value::List(vec![Value::List(vec![s("Bob")])]))]),
            rec(&[("x", Value::List(vec![Value::List(vec![s("Bob")])]))]),
            rec(&[("x", Value::List(vec![Value::List(vec![s("Bob")])]))]),
        ])
    );
    assert_eq!(
        value("users { x = [manager.manager.name] }"),
        Value::List(vec![
            rec(&[("x", Value::List(vec![s("Bob")]))]),
            rec(&[("x", Value::List(vec![s("Bob")]))]),
            rec(&[("x", Value::List(vec![s("Bob")]))]),
        ])
    );
    assert_eq!(
        value("users { x = [mentor?.name] }"),
        Value::List(vec![
            rec(&[("x", Value::List(vec![Value::Null]))]),
            rec(&[("x", Value::List(vec![s("Ada")]))]),
            rec(&[("x", Value::List(vec![Value::Null]))]),
        ])
    );
}

#[test]
fn slice_temp_method_chains() {
    assert_eq!(
        value("users { x = [{a = age}, {a = age + 1}].filter(true).take(1) }"),
        Value::List(vec![
            rec(&[("x", Value::List(vec![rec(&[("a", Value::Int(30))])]))]),
            rec(&[("x", Value::List(vec![rec(&[("a", Value::Int(40))])]))]),
            rec(&[("x", Value::List(vec![rec(&[("a", Value::Int(50))])]))]),
        ])
    );
    assert_eq!(
        value("users { x = [age, age + 1, age + 2].sort_desc(age).take(1) }"),
        Value::List(vec![
            rec(&[("x", Value::List(vec![Value::Int(32)]))]),
            rec(&[("x", Value::List(vec![Value::Int(42)]))]),
            rec(&[("x", Value::List(vec![Value::Int(52)]))]),
        ])
    );
    assert_eq!(
        value("users { x = [age, age + 1, age + 2].take(2).len() }"),
        Value::List(vec![
            rec(&[("x", Value::Int(2))]),
            rec(&[("x", Value::Int(2))]),
            rec(&[("x", Value::Int(2))]),
        ])
    );
}

#[test]
fn slice_chained_scalar_temp() {
    assert_eq!(
        value("users { x = [age, age + 1, age + 2].take(2).sum() }"),
        Value::List(vec![
            rec(&[("x", Value::Int(61))]),
            rec(&[("x", Value::Int(81))]),
            rec(&[("x", Value::Int(101))]),
        ])
    );
    assert_eq!(
        value("users { x = [age + 2, age].sort_asc(age).first() }"),
        Value::List(vec![
            rec(&[("x", Value::Int(30))]),
            rec(&[("x", Value::Int(40))]),
            rec(&[("x", Value::Int(50))]),
        ])
    );
    assert_eq!(
        value("users { x = [age, age + 1, age + 2].take(2).max() }"),
        Value::List(vec![
            rec(&[("x", Value::Int(31))]),
            rec(&[("x", Value::Int(41))]),
            rec(&[("x", Value::Int(51))]),
        ])
    );
}

#[test]
fn slice_temp_element_filter() {
    assert_eq!(
        value("users { x = [{a = age}, {a = age + 1}].filter(a > 30) }"),
        Value::List(vec![
            rec(&[("x", Value::List(vec![rec(&[("a", Value::Int(31))])]))]),
            rec(&[(
                "x",
                Value::List(vec![
                    rec(&[("a", Value::Int(40))]),
                    rec(&[("a", Value::Int(41))]),
                ]),
            )]),
            rec(&[(
                "x",
                Value::List(vec![
                    rec(&[("a", Value::Int(50))]),
                    rec(&[("a", Value::Int(51))]),
                ]),
            )]),
        ])
    );
    assert_eq!(
        value("users { x = [{a = age, b = age + 10}].filter(b > 40) }"),
        Value::List(vec![
            rec(&[("x", Value::List(vec![]))]),
            rec(&[(
                "x",
                Value::List(vec![rec(&[("a", Value::Int(40)), ("b", Value::Int(50))])]),
            )]),
            rec(&[(
                "x",
                Value::List(vec![rec(&[("a", Value::Int(50)), ("b", Value::Int(60))])]),
            )]),
        ])
    );
    assert_eq!(
        value("users { x = [{a = age + 1}, {a = age}].sort_asc(a) }"),
        Value::List(vec![
            rec(&[(
                "x",
                Value::List(vec![
                    rec(&[("a", Value::Int(30))]),
                    rec(&[("a", Value::Int(31))]),
                ]),
            )]),
            rec(&[(
                "x",
                Value::List(vec![
                    rec(&[("a", Value::Int(40))]),
                    rec(&[("a", Value::Int(41))]),
                ]),
            )]),
            rec(&[(
                "x",
                Value::List(vec![
                    rec(&[("a", Value::Int(50))]),
                    rec(&[("a", Value::Int(51))]),
                ]),
            )]),
        ])
    );
}

#[test]
fn slice_temp_scope_needles() {
    assert_eq!(
        value("users { x = [age, age + 1].contains(age + 1) }"),
        Value::List(vec![
            rec(&[("x", Value::Bool(true))]),
            rec(&[("x", Value::Bool(true))]),
            rec(&[("x", Value::Bool(true))]),
        ])
    );
    assert_eq!(
        value("users { x = [age, age + 1].contains(40) }"),
        Value::List(vec![
            rec(&[("x", Value::Bool(false))]),
            rec(&[("x", Value::Bool(true))]),
            rec(&[("x", Value::Bool(false))]),
        ])
    );
    assert_eq!(
        value("users { n = age } { x = [n, n + 1].contains(n) }"),
        Value::List(vec![
            rec(&[("x", Value::Bool(true))]),
            rec(&[("x", Value::Bool(true))]),
            rec(&[("x", Value::Bool(true))]),
        ])
    );
}

#[test]
fn slice_composite_roots() {
    assert_eq!(
        value("{ xs = [[1]] }"),
        Value::Record(vec![(
            "xs".to_string(),
            Value::List(vec![Value::List(vec![Value::Int(1)])]),
        )])
    );
    assert_eq!(
        value("(1, [[2]])"),
        Value::Tuple(vec![
            Value::Int(1),
            Value::List(vec![Value::List(vec![Value::Int(2)])]),
        ])
    );
    assert_eq!(
        value("{ x = 1, y = users.orders }"),
        Value::Record(vec![
            ("x".to_string(), Value::Int(1)),
            (
                "y".to_string(),
                Value::List(vec![
                    Value::List(vec![rec(&[("total", n("10"))]), rec(&[("total", n("20"))]),]),
                    Value::List(vec![rec(&[("total", n("30"))])]),
                    Value::List(vec![]),
                ]),
            ),
        ])
    );
    assert_eq!(
        value("(1, users.orders)"),
        Value::Tuple(vec![
            Value::Int(1),
            Value::List(vec![
                Value::List(vec![rec(&[("total", n("10"))]), rec(&[("total", n("20"))]),]),
                Value::List(vec![rec(&[("total", n("30"))])]),
                Value::List(vec![]),
            ]),
        ])
    );
    assert_eq!(
        value("{ a = users.scores }"),
        Value::Record(vec![(
            "a".to_string(),
            Value::List(vec![
                Value::List(vec![Value::Int(5), Value::Int(7)]),
                Value::List(vec![Value::Int(9)]),
                Value::List(vec![]),
            ]),
        )])
    );
}

#[test]
fn slice_if_branch_nesting() {
    assert_eq!(
        value("if true { users.orders } else { users.orders }"),
        Value::List(vec![
            Value::List(vec![rec(&[("total", n("10"))]), rec(&[("total", n("20"))]),]),
            Value::List(vec![rec(&[("total", n("30"))])]),
            Value::List(vec![]),
        ])
    );
    assert_eq!(
        value("users.orders"),
        Value::List(vec![
            Value::List(vec![rec(&[("total", n("10"))]), rec(&[("total", n("20"))]),]),
            Value::List(vec![rec(&[("total", n("30"))])]),
            Value::List(vec![]),
        ])
    );
    assert_eq!(
        value("if true { [[1, 2]] } else { [[3]] }"),
        Value::List(vec![Value::List(vec![Value::Int(1), Value::Int(2)])])
    );
    assert_eq!(
        value("if false { [[1, 2]] } else { [[3]] }"),
        Value::List(vec![Value::List(vec![Value::Int(3)])])
    );
    assert_eq!(
        value("if true { [(1, [2])] } else { [(3, [4])] }"),
        Value::List(vec![Value::Tuple(vec![
            Value::Int(1),
            Value::List(vec![Value::Int(2)]),
        ])])
    );
    assert_eq!(
        value("if false { [(1, [2])] } else { [(3, [4])] }"),
        Value::List(vec![Value::Tuple(vec![
            Value::Int(3),
            Value::List(vec![Value::Int(4)]),
        ])])
    );
    assert_eq!(
        value("if true { [{a = [1]}] } else { [{a = [2]}] }"),
        Value::List(vec![Value::Record(vec![(
            "a".to_string(),
            Value::List(vec![Value::Int(1)]),
        )])])
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
            Value::List(vec![Value::Int(2), Value::Int(3)]),
        ])
    );
    assert_eq!(
        value("{ a = [1, 2] }"),
        Value::Record(vec![(
            "a".to_string(),
            Value::List(vec![Value::Int(1), Value::Int(2)]),
        )])
    );
}

#[test]
fn slice_tuple_comparison() {
    assert_eq!(value("(1, 2) == (1, 2)"), Value::Bool(true));
    assert_eq!(value("(1, 2) == (1, 3)"), Value::Bool(false));
    assert_eq!(value("(1, 2) != (1, 3)"), Value::Bool(true));
    assert_eq!(value("(1, 2) != (1, 2)"), Value::Bool(false));
    assert_eq!(value("(1, 2) < (3, 4)"), Value::Bool(true));
    assert_eq!(value("(1, 2) < (1, 2)"), Value::Bool(false));
    assert_eq!(value("(1, 2) < (1, 3)"), Value::Bool(true));
    assert_eq!(value("(1, 2) <= (1, 2)"), Value::Bool(true));
    assert_eq!(value("(3, 4) > (1, 9)"), Value::Bool(true));
    assert_eq!(value("(1, 2) >= (2, 1)"), Value::Bool(false));
    assert_eq!(value("(1, 2) in ((1, 2), (3, 4))"), Value::Bool(true));
    assert_eq!(value("(5, 6) in ((1, 2), (3, 4))"), Value::Bool(false));
    assert_eq!(
        value("users[(age, 1) == (30, 1)] { name }"),
        Value::List(vec![rec(&[("name", s("Ada"))])])
    );
    assert_eq!(
        value("users[(age, 99) == (30, 1)] { name }"),
        Value::List(vec![])
    );
    assert_eq!(
        value("users[pos == (1.0f, 2.0f)] { name }"),
        Value::List(vec![rec(&[("name", s("Ada"))])])
    );
    assert_eq!(
        value("users[pos < (3.0f, 4.0f)] { name }"),
        Value::List(vec![rec(&[("name", s("Ada"))])])
    );
    assert_eq!(
        value("users[(nickname, age) == (nickname, age)] { name }"),
        Value::List(vec![
            rec(&[("name", s("Ada"))]),
            rec(&[("name", s("Bob"))]),
            rec(&[("name", s("Cyd"))]),
        ])
    );
    assert_eq!(
        value("(if true { (1, 2) } else { (3, 4) }) == (1, 2)"),
        Value::Bool(true)
    );
    assert_eq!(
        value("users { t = (age, 1) }[t == (30, 1)] { t }"),
        Value::List(vec![Value::Record(vec![(
            "t".to_string(),
            Value::Tuple(vec![Value::Int(30), Value::Int(1)]),
        )])])
    );
}

#[test]
fn slice_record_comparison() {
    assert_eq!(
        value("users[manager == manager] { name }"),
        Value::List(vec![
            rec(&[("name", s("Ada"))]),
            rec(&[("name", s("Bob"))]),
            rec(&[("name", s("Cyd"))]),
        ])
    );
    assert_eq!(
        value("users[manager != manager] { name }"),
        Value::List(vec![])
    );
    assert_eq!(
        value("users[mentor == mentor] { name }"),
        Value::List(vec![
            rec(&[("name", s("Ada"))]),
            rec(&[("name", s("Bob"))]),
            rec(&[("name", s("Cyd"))]),
        ])
    );
    assert_eq!(
        value(r#"users[manager == { name = "Bob" }] { name }"#),
        Value::List(vec![
            rec(&[("name", s("Ada"))]),
            rec(&[("name", s("Bob"))]),
            rec(&[("name", s("Cyd"))]),
        ])
    );
    assert_eq!(
        value(r#"users[manager == { name = "Nobody" }] { name }"#),
        Value::List(vec![])
    );
    assert_eq!(
        value("users { x = ({ n = name } == { n = name }) }"),
        Value::List(vec![
            rec(&[("x", Value::Bool(true))]),
            rec(&[("x", Value::Bool(true))]),
            rec(&[("x", Value::Bool(true))]),
        ])
    );
    assert_eq!(
        value(r#"users { x = ({ n = name } == { n = "zzz" }) }"#),
        Value::List(vec![
            rec(&[("x", Value::Bool(false))]),
            rec(&[("x", Value::Bool(false))]),
            rec(&[("x", Value::Bool(false))]),
        ])
    );
}

#[test]
fn slice_record_spines() {
    assert_eq!(
        value("users[manager.manager == manager] { name }"),
        Value::List(vec![
            rec(&[("name", s("Ada"))]),
            rec(&[("name", s("Bob"))]),
            rec(&[("name", s("Cyd"))]),
        ])
    );
    assert_eq!(
        value("users[mentor?.manager == mentor?.manager] { name }"),
        Value::List(vec![
            rec(&[("name", s("Ada"))]),
            rec(&[("name", s("Bob"))]),
            rec(&[("name", s("Cyd"))]),
        ])
    );
    assert_eq!(
        value("users[mentor?.manager == mentor] { name }"),
        Value::List(vec![rec(&[("name", s("Ada"))]), rec(&[("name", s("Cyd"))]),])
    );
    assert_eq!(
        value(r#"users[manager.manager == { name = "Bob" }] { name }"#),
        Value::List(vec![
            rec(&[("name", s("Ada"))]),
            rec(&[("name", s("Bob"))]),
            rec(&[("name", s("Cyd"))]),
        ])
    );
    assert_eq!(
        value(r#"users[manager.manager == { name = "Zelda" }] { name }"#),
        Value::List(vec![])
    );
}

#[test]
fn slice_list_comparison() {
    assert_eq!(value("[1, 2] == [1, 2]"), Value::Bool(true));
    assert_eq!(value("[1, 2] == [1, 2, 3]"), Value::Bool(false));
    assert_eq!(value("[1, 2] == [2, 1]"), Value::Bool(false));
    assert_eq!(value("[1, 2] != [2, 1]"), Value::Bool(true));
    assert_eq!(value("[[1]] == [[1]]"), Value::Bool(true));
    assert_eq!(value("[(1, 2)] == [(2, 1)]"), Value::Bool(false));
    assert_eq!(
        value("users[orders == orders] { name }"),
        Value::List(vec![
            rec(&[("name", s("Ada"))]),
            rec(&[("name", s("Bob"))]),
            rec(&[("name", s("Cyd"))]),
        ])
    );
    assert_eq!(
        value("users[orders != orders] { name }"),
        Value::List(vec![])
    );
    assert_eq!(
        value("users[tags == tags] { name }"),
        Value::List(vec![
            rec(&[("name", s("Ada"))]),
            rec(&[("name", s("Bob"))]),
            rec(&[("name", s("Cyd"))]),
        ])
    );
    assert_eq!(
        value("users[orders.take(1) == orders.take(2)] { name }"),
        Value::List(vec![rec(&[("name", s("Bob"))]), rec(&[("name", s("Cyd"))])])
    );
    assert_eq!(
        value("(if true { [1] } else { [2] }) == [1]"),
        Value::Bool(true)
    );
    assert_eq!(
        value("(if false { [1] } else { [2] }) == [1]"),
        Value::Bool(false)
    );
}

#[test]
fn slice_over_collection_nested() {
    assert_eq!(
        value("users { s = orders.sum_over(manager.orders.len()) }"),
        Value::List(vec![
            rec(&[("s", Value::Int(2))]),
            rec(&[("s", Value::Int(1))]),
            rec(&[("s", Value::Null)]),
        ])
    );
    assert_eq!(
        value("users { s = orders.sum_over(manager.tags.len()) }"),
        Value::List(vec![
            rec(&[("s", Value::Int(2))]),
            rec(&[("s", Value::Int(1))]),
            rec(&[("s", Value::Null)]),
        ])
    );
    assert_eq!(
        value("users { s = orders.sum_over(tags.len()) }"),
        Value::List(vec![
            rec(&[("s", Value::Int(4))]),
            rec(&[("s", Value::Int(1))]),
            rec(&[("s", Value::Null)]),
        ])
    );
    assert_eq!(
        value("users { a = orders.avg_over(manager.orders.len()) }"),
        Value::List(vec![
            rec(&[("a", Value::Float(1.0))]),
            rec(&[("a", Value::Float(1.0))]),
            rec(&[("a", Value::Null)]),
        ])
    );
    assert_eq!(
        value("users { s = orders.sum_over(manager.orders.take(1).len()) }"),
        Value::List(vec![
            rec(&[("s", Value::Int(2))]),
            rec(&[("s", Value::Int(1))]),
            rec(&[("s", Value::Null)]),
        ])
    );
    assert_eq!(
        value("users { s = orders.sum_over(manager.orders.skip(1).len()) }"),
        Value::List(vec![
            rec(&[("s", Value::Int(0))]),
            rec(&[("s", Value::Int(0))]),
            rec(&[("s", Value::Null)]),
        ])
    );
    assert_eq!(
        value("users { s = orders.sum_over(manager.manager.orders.len()) }"),
        Value::List(vec![
            rec(&[("s", Value::Int(2))]),
            rec(&[("s", Value::Int(1))]),
            rec(&[("s", Value::Null)]),
        ])
    );
    assert_eq!(
        value("users { s = orders.take(age).sum_over(manager.orders.len()) }"),
        Value::List(vec![
            rec(&[("s", Value::Int(2))]),
            rec(&[("s", Value::Int(1))]),
            rec(&[("s", Value::Null)]),
        ])
    );
    assert_eq!(
        value("users { s = orders.take(age).sum_over(tags.len()) }"),
        Value::List(vec![
            rec(&[("s", Value::Int(4))]),
            rec(&[("s", Value::Int(1))]),
            rec(&[("s", Value::Null)]),
        ])
    );
}

#[test]
fn slice_take_reduction() {
    assert_eq!(
        value("users { s = scores.take(1).sum() }"),
        Value::List(vec![
            rec(&[("s", Value::Int(5))]),
            rec(&[("s", Value::Int(9))]),
            rec(&[("s", Value::Null)]),
        ])
    );
    assert_eq!(
        value("users { s = scores.skip(1).sum() }"),
        Value::List(vec![
            rec(&[("s", Value::Int(7))]),
            rec(&[("s", Value::Null)]),
            rec(&[("s", Value::Null)]),
        ])
    );
    assert_eq!(
        value("users { s = scores.take(1).len() }"),
        Value::List(vec![
            rec(&[("s", Value::Int(1))]),
            rec(&[("s", Value::Int(1))]),
            rec(&[("s", Value::Int(0))]),
        ])
    );
    assert_eq!(
        value(r#"users { c = tags.take(1).contains("x") }"#),
        Value::List(vec![
            rec(&[("c", Value::Bool(true))]),
            rec(&[("c", Value::Bool(false))]),
            rec(&[("c", Value::Bool(false))]),
        ])
    );
    assert_eq!(
        value(r#"users { c = tags.skip(1).contains("y") }"#),
        Value::List(vec![
            rec(&[("c", Value::Bool(true))]),
            rec(&[("c", Value::Bool(false))]),
            rec(&[("c", Value::Bool(false))]),
        ])
    );
    assert_eq!(
        value("users { c = scores.take(1).contains(5) }"),
        Value::List(vec![
            rec(&[("c", Value::Bool(true))]),
            rec(&[("c", Value::Bool(false))]),
            rec(&[("c", Value::Bool(false))]),
        ])
    );
    assert_eq!(
        value("users { t = orders.take(1) { total }.take(1) }"),
        Value::List(vec![
            rec(&[("t", Value::List(vec![rec(&[("total", n("10"))])]))]),
            rec(&[("t", Value::List(vec![rec(&[("total", n("30"))])]))]),
            rec(&[("t", Value::List(vec![]))]),
        ])
    );
    assert_eq!(
        value(r#"users { c = tags.contains(name) }"#),
        Value::List(vec![
            rec(&[("c", Value::Bool(false))]),
            rec(&[("c", Value::Bool(false))]),
            rec(&[("c", Value::Bool(false))]),
        ])
    );
    assert_eq!(
        value(r#"users { c = tags.skip(1).contains(name) }"#),
        Value::List(vec![
            rec(&[("c", Value::Bool(false))]),
            rec(&[("c", Value::Bool(false))]),
            rec(&[("c", Value::Bool(false))]),
        ])
    );
    assert_eq!(
        value("users { c = [30].contains(age) }"),
        Value::List(vec![
            rec(&[("c", Value::Bool(true))]),
            rec(&[("c", Value::Bool(false))]),
            rec(&[("c", Value::Bool(false))]),
        ])
    );
}

#[test]
fn slice_take_prefixed_collapse() {
    assert_eq!(
        value("users { o = orders.take(age).first() }"),
        Value::List(vec![
            rec(&[("o", rec(&[("total", n("10"))]))]),
            rec(&[("o", rec(&[("total", n("30"))]))]),
            rec(&[("o", Value::Null)]),
        ])
    );
    assert_eq!(
        value("users { o = orders.take(age - 29).first() }"),
        Value::List(vec![
            rec(&[("o", rec(&[("total", n("10"))]))]),
            rec(&[("o", rec(&[("total", n("30"))]))]),
            rec(&[("o", Value::Null)]),
        ])
    );
    assert_eq!(
        value("users { o = orders.take(age).nth(1) }"),
        Value::List(vec![
            rec(&[("o", rec(&[("total", n("20"))]))]),
            rec(&[("o", Value::Null)]),
            rec(&[("o", Value::Null)]),
        ])
    );
    assert_eq!(
        value("users { o = orders.skip(age - 30).first() }"),
        Value::List(vec![
            rec(&[("o", rec(&[("total", n("10"))]))]),
            rec(&[("o", Value::Null)]),
            rec(&[("o", Value::Null)]),
        ])
    );
    assert_eq!(
        value("users { o = orders.take(age).nth(age - 29) }"),
        Value::List(vec![
            rec(&[("o", rec(&[("total", n("20"))]))]),
            rec(&[("o", Value::Null)]),
            rec(&[("o", Value::Null)]),
        ])
    );
    assert_eq!(
        value("users { o = orders.take(age).skip(1).first() }"),
        Value::List(vec![
            rec(&[("o", rec(&[("total", n("20"))]))]),
            rec(&[("o", Value::Null)]),
            rec(&[("o", Value::Null)]),
        ])
    );
    assert_eq!(
        value("users { o = orders.take(age).filter(age > 30).first() }"),
        Value::List(vec![
            rec(&[("o", Value::Null)]),
            rec(&[("o", rec(&[("total", n("30"))]))]),
            rec(&[("o", Value::Null)]),
        ])
    );
    assert_eq!(
        value("users { o = orders.sort(age).first() }"),
        Value::List(vec![
            rec(&[("o", rec(&[("total", n("10"))]))]),
            rec(&[("o", rec(&[("total", n("30"))]))]),
            rec(&[("o", Value::Null)]),
        ])
    );
    assert_eq!(
        value("users { t = orders.sort(age).first()?.total }"),
        Value::List(vec![
            rec(&[("t", n("10"))]),
            rec(&[("t", n("30"))]),
            rec(&[("t", Value::Null)]),
        ])
    );
}

#[test]
fn slice_array_temp_predicate_sort() {
    assert_eq!(
        value(r#"users.filter([age].contains(30)) { name }"#),
        Value::List(vec![rec(&[("name", s("Ada"))])])
    );
    assert_eq!(
        value(r#"users { big = [age].contains(30) }"#),
        Value::List(vec![
            rec(&[("big", Value::Bool(true))]),
            rec(&[("big", Value::Bool(false))]),
            rec(&[("big", Value::Bool(false))]),
        ])
    );
    assert_eq!(
        value("users.sort([age, age + 1].take(age - 30).len()) { name }"),
        Value::List(vec![
            rec(&[("name", s("Ada"))]),
            rec(&[("name", s("Bob"))]),
            rec(&[("name", s("Cyd"))]),
        ])
    );
}

#[test]
fn slice_scalar_first_storage() {
    assert_eq!(
        value(r#"users { c = tags.first() }"#),
        Value::List(vec![
            rec(&[("c", s("x"))]),
            rec(&[("c", s("z"))]),
            rec(&[("c", Value::Null)]),
        ])
    );
    assert_eq!(
        value(r#"users { c = tags.take(1).first() }"#),
        Value::List(vec![
            rec(&[("c", s("x"))]),
            rec(&[("c", s("z"))]),
            rec(&[("c", Value::Null)]),
        ])
    );
    assert_eq!(
        value(r#"users { c = tags.skip(1).first() }"#),
        Value::List(vec![
            rec(&[("c", s("y"))]),
            rec(&[("c", Value::Null)]),
            rec(&[("c", Value::Null)]),
        ])
    );
    assert_eq!(
        value("users { x = [age, age + 1].nth(age - 30) }"),
        Value::List(vec![
            rec(&[("x", Value::Int(30))]),
            rec(&[("x", Value::Null)]),
            rec(&[("x", Value::Null)]),
        ])
    );
    assert_eq!(
        value("users { x = [if true {(1, 2)} else {(3, 4)}] }"),
        Value::List(vec![
            rec(&[(
                "x",
                Value::List(vec![Value::Tuple(vec![Value::Int(1), Value::Int(2)])]),
            )]),
            rec(&[(
                "x",
                Value::List(vec![Value::Tuple(vec![Value::Int(1), Value::Int(2)])]),
            )]),
            rec(&[(
                "x",
                Value::List(vec![Value::Tuple(vec![Value::Int(1), Value::Int(2)])]),
            )]),
        ])
    );
    assert_eq!(
        value("users { x = [if active {(age, 1)} else {(0, 0)}] }"),
        Value::List(vec![
            rec(&[(
                "x",
                Value::List(vec![Value::Tuple(vec![Value::Int(30), Value::Int(1)])]),
            )]),
            rec(&[(
                "x",
                Value::List(vec![Value::Tuple(vec![Value::Int(0), Value::Int(0)])]),
            )]),
            rec(&[(
                "x",
                Value::List(vec![Value::Tuple(vec![Value::Int(50), Value::Int(1)])]),
            )]),
        ])
    );
}

#[test]
fn slice_temp_take_first() {
    assert_eq!(
        value("users { t = orders.take(age).first()?.total }"),
        Value::List(vec![
            rec(&[("t", n("10"))]),
            rec(&[("t", n("30"))]),
            rec(&[("t", Value::Null)]),
        ])
    );
    assert_eq!(
        value("users { t = orders.take(age).nth(1)?.total }"),
        Value::List(vec![
            rec(&[("t", n("20"))]),
            rec(&[("t", Value::Null)]),
            rec(&[("t", Value::Null)]),
        ])
    );
}

#[test]
fn slice_temp_take_predicate_sort() {
    assert_eq!(
        value("users.filter(orders.take(age).len() > 1) { name }"),
        Value::List(vec![rec(&[("name", s("Ada"))])])
    );
    assert_eq!(
        value("users { name, big = orders.take(age).len() > 1 }"),
        Value::List(vec![
            rec(&[("name", s("Ada")), ("big", Value::Bool(true))]),
            rec(&[("name", s("Bob")), ("big", Value::Bool(false))]),
            rec(&[("name", s("Cyd")), ("big", Value::Bool(false))]),
        ])
    );
    assert_eq!(
        value("users.sort(orders.take(age).len()) { name }"),
        Value::List(vec![
            rec(&[("name", s("Cyd"))]),
            rec(&[("name", s("Bob"))]),
            rec(&[("name", s("Ada"))]),
        ])
    );
    assert_eq!(
        value("users.sort_desc(orders.take(age).len()) { name }"),
        Value::List(vec![
            rec(&[("name", s("Ada"))]),
            rec(&[("name", s("Bob"))]),
            rec(&[("name", s("Cyd"))]),
        ])
    );
    assert_eq!(
        value("users.filter(orders.take(age).len() > age - 29) { name }"),
        Value::List(vec![rec(&[("name", s("Ada"))])])
    );
}

#[test]
fn slice_list_mixed() {
    assert_eq!(
        value(r#"users[tags == ["x", "y"]] { name }"#),
        Value::List(vec![rec(&[("name", s("Ada"))])])
    );
    assert_eq!(
        value(r#"users[tags == ["z"]] { name }"#),
        Value::List(vec![rec(&[("name", s("Bob"))])])
    );
    assert_eq!(
        value(r#"users[tags == ["zzz"]] { name }"#),
        Value::List(vec![])
    );
    assert_eq!(
        value(r#"users[["x", "y"] == tags] { name }"#),
        Value::List(vec![rec(&[("name", s("Ada"))])])
    );
    assert_eq!(
        value("users[scores == [5, 7]] { name }"),
        Value::List(vec![rec(&[("name", s("Ada"))])])
    );
    assert_eq!(
        value(r#"users[tags.take(1) == ["x"]] { name }"#),
        Value::List(vec![rec(&[("name", s("Ada"))])])
    );
}

#[test]
fn slice_composite_contains() {
    assert_eq!(value("[(1, 2)].contains((1, 2))"), Value::Bool(true));
    assert_eq!(value("[(1, 2)].contains((3, 4))"), Value::Bool(false));
    assert_eq!(
        value("[(1, 2), (3, 4)].contains((3, 4))"),
        Value::Bool(true)
    );
    assert_eq!(value("[{ a = 1 }].contains({ a = 1 })"), Value::Bool(true));
    assert_eq!(
        value("[(1, 2)].take(1).contains((1, 2))"),
        Value::Bool(true)
    );
    assert_eq!(
        value("users { x = [(age, 1)].contains((30, 1)) }"),
        Value::List(vec![
            rec(&[("x", Value::Bool(true))]),
            rec(&[("x", Value::Bool(false))]),
            rec(&[("x", Value::Bool(false))]),
        ])
    );
    assert_eq!(
        value("users[orders.contains({ total = 10.0 })] { name }"),
        Value::List(vec![rec(&[("name", s("Ada"))])])
    );
    assert_eq!(
        value("users[orders.contains({ total = 99.0 })] { name }"),
        Value::List(vec![])
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
        Value::List(vec![Value::Int(1)])
    );
    assert_eq!(
        value("if true { (1, 2) } else { (3, 4) }"),
        Value::Tuple(vec![Value::Int(1), Value::Int(2)])
    );
    assert_eq!(
        value("users { x = if true { [1] } else { [2] } }"),
        Value::List(vec![
            rec(&[("x", Value::List(vec![Value::Int(1)]))]),
            rec(&[("x", Value::List(vec![Value::Int(1)]))]),
            rec(&[("x", Value::List(vec![Value::Int(1)]))]),
        ])
    );
}

#[test]
fn slice_query_valued_elements() {
    let nested = || {
        Value::List(vec![
            rec(&[("x", Value::List(vec![Value::List(vec![Value::Int(1)])]))]),
            rec(&[("x", Value::List(vec![Value::List(vec![Value::Int(1)])]))]),
            rec(&[("x", Value::List(vec![Value::List(vec![Value::Int(1)])]))]),
        ])
    };
    assert_eq!(value("users { x = [[1].take(1)] }"), nested());
    assert_eq!(
        value("users { x = [[1].take(0)] }"),
        Value::List(vec![
            rec(&[("x", Value::List(vec![Value::List(vec![])]))]),
            rec(&[("x", Value::List(vec![Value::List(vec![])]))]),
            rec(&[("x", Value::List(vec![Value::List(vec![])]))]),
        ])
    );
    assert_eq!(
        value("users { x = [[1].take(1), [2, 3].take(1)] }"),
        Value::List(vec![
            rec(&[(
                "x",
                Value::List(vec![
                    Value::List(vec![Value::Int(1)]),
                    Value::List(vec![Value::Int(2)]),
                ]),
            )]),
            rec(&[(
                "x",
                Value::List(vec![
                    Value::List(vec![Value::Int(1)]),
                    Value::List(vec![Value::Int(2)]),
                ]),
            )]),
            rec(&[(
                "x",
                Value::List(vec![
                    Value::List(vec![Value::Int(1)]),
                    Value::List(vec![Value::Int(2)]),
                ]),
            )]),
        ])
    );
    assert_eq!(
        value("users { x = [[1].take(1)].first() }"),
        Value::List(vec![
            rec(&[("x", Value::List(vec![Value::Int(1)]))]),
            rec(&[("x", Value::List(vec![Value::Int(1)]))]),
            rec(&[("x", Value::List(vec![Value::Int(1)]))]),
        ])
    );
    assert_eq!(
        value("users { x = [(users.first()?.name)].first() }"),
        Value::List(vec![
            rec(&[("x", s("Ada"))]),
            rec(&[("x", s("Ada"))]),
            rec(&[("x", s("Ada"))]),
        ])
    );
    assert_eq!(
        value("[users.first()]"),
        Value::List(vec![value("users.first()")])
    );
    assert_eq!(
        value("[configs.take(1)]"),
        Value::List(vec![value("configs.take(1)")])
    );
    assert_eq!(
        value("[[users.first()]]"),
        Value::List(vec![value("[users.first()]")])
    );
    assert_eq!(
        value("[[(1, 2)].take(1)]"),
        Value::List(vec![Value::List(vec![Value::Tuple(vec![
            Value::Int(1),
            Value::Int(2),
        ])])])
    );
    assert_eq!(
        value("[[(1, 2)].first()]"),
        Value::List(vec![Value::Tuple(vec![Value::Int(1), Value::Int(2)])])
    );
}

#[test]
fn slice_multi_list_fork() {
    let ab = |a: Value, b: Value| {
        Value::List(vec![rec(&[
            ("a", Value::List(vec![a])),
            ("b", Value::List(vec![b])),
        ])])
    };
    assert_eq!(
        value("users { x = [{a = [age], b = [1]}] }"),
        Value::List(vec![
            rec(&[("x", ab(Value::Int(30), Value::Int(1)))]),
            rec(&[("x", ab(Value::Int(40), Value::Int(1)))]),
            rec(&[("x", ab(Value::Int(50), Value::Int(1)))]),
        ])
    );
    assert_eq!(
        value("users { x = [([age], [1])] }"),
        Value::List(vec![
            rec(&[(
                "x",
                Value::List(vec![Value::Tuple(vec![
                    Value::List(vec![Value::Int(30)]),
                    Value::List(vec![Value::Int(1)]),
                ])]),
            )]),
            rec(&[(
                "x",
                Value::List(vec![Value::Tuple(vec![
                    Value::List(vec![Value::Int(40)]),
                    Value::List(vec![Value::Int(1)]),
                ])]),
            )]),
            rec(&[(
                "x",
                Value::List(vec![Value::Tuple(vec![
                    Value::List(vec![Value::Int(50)]),
                    Value::List(vec![Value::Int(1)]),
                ])]),
            )]),
        ])
    );
    assert_eq!(
        value("users { x = [{n = name, a = [age], b = [1]}] }"),
        Value::List(vec![
            rec(&[(
                "x",
                Value::List(vec![rec(&[
                    ("n", s("Ada")),
                    ("a", Value::List(vec![Value::Int(30)])),
                    ("b", Value::List(vec![Value::Int(1)])),
                ])]),
            )]),
            rec(&[(
                "x",
                Value::List(vec![rec(&[
                    ("n", s("Bob")),
                    ("a", Value::List(vec![Value::Int(40)])),
                    ("b", Value::List(vec![Value::Int(1)])),
                ])]),
            )]),
            rec(&[(
                "x",
                Value::List(vec![rec(&[
                    ("n", s("Cyd")),
                    ("a", Value::List(vec![Value::Int(50)])),
                    ("b", Value::List(vec![Value::Int(1)])),
                ])]),
            )]),
        ])
    );
    assert_eq!(
        value("users { x = [{a = [age], b = [age], c = [1]}] }"),
        Value::List(vec![
            rec(&[(
                "x",
                Value::List(vec![rec(&[
                    ("a", Value::List(vec![Value::Int(30)])),
                    ("b", Value::List(vec![Value::Int(30)])),
                    ("c", Value::List(vec![Value::Int(1)])),
                ])]),
            )]),
            rec(&[(
                "x",
                Value::List(vec![rec(&[
                    ("a", Value::List(vec![Value::Int(40)])),
                    ("b", Value::List(vec![Value::Int(40)])),
                    ("c", Value::List(vec![Value::Int(1)])),
                ])]),
            )]),
            rec(&[(
                "x",
                Value::List(vec![rec(&[
                    ("a", Value::List(vec![Value::Int(50)])),
                    ("b", Value::List(vec![Value::Int(50)])),
                    ("c", Value::List(vec![Value::Int(1)])),
                ])]),
            )]),
        ])
    );
    assert_eq!(
        value("users { x = [{a = [[age]], b = [1]}] }"),
        Value::List(vec![
            rec(&[(
                "x",
                Value::List(vec![rec(&[
                    ("a", Value::List(vec![Value::List(vec![Value::Int(30)])])),
                    ("b", Value::List(vec![Value::Int(1)])),
                ])]),
            )]),
            rec(&[(
                "x",
                Value::List(vec![rec(&[
                    ("a", Value::List(vec![Value::List(vec![Value::Int(40)])])),
                    ("b", Value::List(vec![Value::Int(1)])),
                ])]),
            )]),
            rec(&[(
                "x",
                Value::List(vec![rec(&[
                    ("a", Value::List(vec![Value::List(vec![Value::Int(50)])])),
                    ("b", Value::List(vec![Value::Int(1)])),
                ])]),
            )]),
        ])
    );
    assert_eq!(
        value("users { x = [{a = [age], b = [1]}].take(1) }"),
        Value::List(vec![
            rec(&[("x", ab(Value::Int(30), Value::Int(1)))]),
            rec(&[("x", ab(Value::Int(40), Value::Int(1)))]),
            rec(&[("x", ab(Value::Int(50), Value::Int(1)))]),
        ])
    );
}

#[test]
fn slice_unwrap_or_presence() {
    assert_eq!(
        value("users { x = [[1], [2]].first().unwrap_or([0]) }"),
        Value::List(vec![
            rec(&[("x", Value::List(vec![Value::Int(1)]))]),
            rec(&[("x", Value::List(vec![Value::Int(1)]))]),
            rec(&[("x", Value::List(vec![Value::Int(1)]))]),
        ])
    );
    assert_eq!(
        value("users { x = [[1]].take(0).first().unwrap_or([0]) }"),
        Value::List(vec![
            rec(&[("x", Value::List(vec![Value::Int(0)]))]),
            rec(&[("x", Value::List(vec![Value::Int(0)]))]),
            rec(&[("x", Value::List(vec![Value::Int(0)]))]),
        ])
    );
    assert_eq!(
        value("users { x = [[(1, 2)]].first().unwrap_or([(0, 0)]) }"),
        Value::List(vec![
            rec(&[(
                "x",
                Value::List(vec![Value::Tuple(vec![Value::Int(1), Value::Int(2)])]),
            )]),
            rec(&[(
                "x",
                Value::List(vec![Value::Tuple(vec![Value::Int(1), Value::Int(2)])]),
            )]),
            rec(&[(
                "x",
                Value::List(vec![Value::Tuple(vec![Value::Int(1), Value::Int(2)])]),
            )]),
        ])
    );
    assert_eq!(
        value("users { x = [[age]].first().unwrap_or([0]) }"),
        Value::List(vec![
            rec(&[("x", Value::List(vec![Value::Int(30)]))]),
            rec(&[("x", Value::List(vec![Value::Int(40)]))]),
            rec(&[("x", Value::List(vec![Value::Int(50)]))]),
        ])
    );
    assert_eq!(
        value("users { x = [[age]].take(0).first().unwrap_or([0]) }"),
        Value::List(vec![
            rec(&[("x", Value::List(vec![Value::Int(0)]))]),
            rec(&[("x", Value::List(vec![Value::Int(0)]))]),
            rec(&[("x", Value::List(vec![Value::Int(0)]))]),
        ])
    );
    assert_eq!(
        value("users { x = [[age]].nth(5).unwrap_or([0]) }"),
        Value::List(vec![
            rec(&[("x", Value::List(vec![Value::Int(0)]))]),
            rec(&[("x", Value::List(vec![Value::Int(0)]))]),
            rec(&[("x", Value::List(vec![Value::Int(0)]))]),
        ])
    );
    let mut rt = fixture();
    let err = rt
        .execute_value(&compile_q("users { x = [[1]].nth(-1).unwrap_or([0]) }"))
        .unwrap_err();
    assert!(
        err.to_string()
            .contains("nth requires a non-negative integer")
    );
    let mut rt = fixture();
    let err = rt
        .execute_value(&compile_q("users { x = [[age]].nth(-1).unwrap_or([0]) }"))
        .unwrap_err();
    assert!(
        err.to_string()
            .contains("nth requires a non-negative integer")
    );
}

#[test]
fn slice_unwrap_or_records() {
    let cfg = |n: &str, o: &str| {
        Value::Record(vec![
            ("name".to_string(), s(n)),
            ("owner".to_string(), s(o)),
        ])
    };
    assert_eq!(
        value("users { x = configs.take(1).first().unwrap_or({name = \"d\", owner = \"e\"}) }"),
        Value::List(vec![
            rec(&[("x", cfg("n1", "a@x"))]),
            rec(&[("x", cfg("n1", "a@x"))]),
            rec(&[("x", cfg("n1", "a@x"))]),
        ])
    );
    assert_eq!(
        value("users { x = configs.take(0).first().unwrap_or({name = \"d\", owner = \"e\"}) }"),
        Value::List(vec![
            rec(&[("x", cfg("d", "e"))]),
            rec(&[("x", cfg("d", "e"))]),
            rec(&[("x", cfg("d", "e"))]),
        ])
    );
    assert_eq!(
        value("users { x = configs.take(1).first().unwrap_or({owner = \"e\", name = \"d\"}) }"),
        Value::List(vec![
            rec(&[("x", cfg("n1", "a@x"))]),
            rec(&[("x", cfg("n1", "a@x"))]),
            rec(&[("x", cfg("n1", "a@x"))]),
        ])
    );
    assert_eq!(
        value("users { x = [[{a = 1}]].first().unwrap_or([{a = 0}]) }"),
        Value::List(vec![
            rec(&[(
                "x",
                Value::List(vec![Value::Record(vec![("a".to_string(), Value::Int(1))])]),
            )]),
            rec(&[(
                "x",
                Value::List(vec![Value::Record(vec![("a".to_string(), Value::Int(1))])]),
            )]),
            rec(&[(
                "x",
                Value::List(vec![Value::Record(vec![("a".to_string(), Value::Int(1))])]),
            )]),
        ])
    );
}

#[test]
fn slice_query_temp_elements() {
    let ord = |t: &str| Value::Record(vec![("total".to_string(), Value::Dec(dec(t)))]);
    assert_eq!(
        value("users { x = [orders.take(1)] }"),
        Value::List(vec![
            rec(&[("x", Value::List(vec![Value::List(vec![ord("10.00")])]))]),
            rec(&[("x", Value::List(vec![Value::List(vec![ord("30.00")])]))]),
            rec(&[("x", Value::List(vec![Value::List(vec![])]))]),
        ])
    );
    assert_eq!(
        value("users { x = [orders.skip(1)] }"),
        Value::List(vec![
            rec(&[("x", Value::List(vec![Value::List(vec![ord("20.00")])]))]),
            rec(&[("x", Value::List(vec![Value::List(vec![])]))]),
            rec(&[("x", Value::List(vec![Value::List(vec![])]))]),
        ])
    );
    assert_eq!(
        value("users { x = [orders] }"),
        Value::List(vec![
            rec(&[(
                "x",
                Value::List(vec![Value::List(vec![ord("10.00"), ord("20.00")])]),
            )]),
            rec(&[("x", Value::List(vec![Value::List(vec![ord("30.00")])]))]),
            rec(&[("x", Value::List(vec![Value::List(vec![])]))]),
        ])
    );
    assert_eq!(
        value("users { x = [orders.first()] }"),
        Value::List(vec![
            rec(&[("x", Value::List(vec![ord("10.00")]))]),
            rec(&[("x", Value::List(vec![ord("30.00")]))]),
            rec(&[(
                "x",
                Value::List(vec![Value::Record(vec![(
                    "total".to_string(),
                    Value::Null,
                )])]),
            )]),
        ])
    );
    assert_eq!(
        value("users { x = [orders.nth(0)] }"),
        Value::List(vec![
            rec(&[("x", Value::List(vec![ord("10.00")]))]),
            rec(&[("x", Value::List(vec![ord("30.00")]))]),
            rec(&[(
                "x",
                Value::List(vec![Value::Record(vec![(
                    "total".to_string(),
                    Value::Null,
                )])]),
            )]),
        ])
    );
    assert_eq!(
        value("users { x = [(orders.first()?.total)] }"),
        Value::List(vec![
            rec(&[("x", Value::List(vec![Value::Dec(dec("10.00"))]))]),
            rec(&[("x", Value::List(vec![Value::Dec(dec("30.00"))]))]),
            rec(&[("x", Value::List(vec![Value::Null]))]),
        ])
    );
    assert_eq!(
        value("users { x = [users.first()] }"),
        Value::List(vec![
            rec(&[("x", Value::List(vec![value("users.first()")]))]),
            rec(&[("x", Value::List(vec![value("users.first()")]))]),
            rec(&[("x", Value::List(vec![value("users.first()")]))]),
        ])
    );
    assert_eq!(
        value("users { x = [configs.take(1)] }"),
        Value::List(vec![
            rec(&[("x", value("[configs.take(1)]"))]),
            rec(&[("x", value("[configs.take(1)]"))]),
            rec(&[("x", value("[configs.take(1)]"))]),
        ])
    );
    let mut rt = fixture();
    let err = rt
        .execute_value(&compile_q("users { x = [orders.take(-1)] }"))
        .unwrap_err();
    assert!(
        err.to_string()
            .contains("take requires a non-negative integer")
    );
}
