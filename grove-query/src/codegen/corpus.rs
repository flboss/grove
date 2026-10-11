use std::collections::HashMap;

use super::model::{CompiledQuery, Param, ParamValue};
use crate::typecheck::typecheck;

const SCHEMA_SRC: &str = r#"
    root users: User;
    root configs: Config;
    root roles: Role;
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
        roles: &List<Role>,
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
    struct Role {
        name: String,
        users: &List<User>,
    }
    rel User.manager <<-> User.subordinates (users.manager_id -> users.id);
    rel User.mentor <<-> User.mentees (users.mentor_id -> users.id);
    rel Order.user <<-> User.orders (orders.user_id -> users.id);
    rel Settings.user <-> User.settings (settings.user_id -> users.id);
    rel Item.order <<-> Order.items (items.order_id -> orders.id);
    rel Role.users <<->> User.roles via user_roles[role_id -> roles.id, user_id -> users.id];
"#;

fn compile(query: &str) -> CompiledQuery {
    let schema = grove_schema::validate(
        grove_schema::parse_schema(SCHEMA_SRC)
            .0
            .expect("invalid test schema"),
    )
    .0
    .expect("invalid test schema");
    let (file, parse_diags) = crate::parse_query(query);
    assert!(
        parse_diags.is_empty(),
        "expected no parse errors, got {parse_diags:?}"
    );
    let (typed, type_diags) = typecheck(file.unwrap(), &schema);
    assert!(
        type_diags.is_empty(),
        "expected no type errors, got {type_diags:?}"
    );
    crate::codegen::codegen(&typed.unwrap(), &schema)
}

fn sql(query: &str) -> (String, Vec<Param>) {
    let mut compiled = compile(query);
    assert_eq!(compiled.statements.len(), 1);
    let stmt = compiled.statements.pop().unwrap();
    (stmt.sql, stmt.params)
}

fn int_param(n: i64) -> Param {
    Param::Value(ParamValue::Int(n))
}

fn limit_msg(method: &str) -> Param {
    Param::Value(ParamValue::String(format!(
        "{method} requires a non-negative integer"
    )))
}

fn case_pos_guard(n: i64, method: &str) -> (String, Vec<Param>) {
    (
        "CASE WHEN ? >= ? THEN ? ELSE \"$$panic\"(?) END".to_string(),
        vec![int_param(n), int_param(0), int_param(n), limit_msg(method)],
    )
}

fn str_param(s: &str) -> Param {
    Param::Value(ParamValue::String(s.to_string()))
}

fn check_rejects(cases: &[(&str, &str)]) {
    let schema = grove_schema::validate(
        grove_schema::parse_schema(SCHEMA_SRC)
            .0
            .expect("invalid test schema"),
    )
    .0
    .expect("invalid test schema");
    for (query, code) in cases {
        let (file, parse_diags) = crate::parse_query(query);
        assert!(
            parse_diags.is_empty(),
            "expected no parse errors, got {parse_diags:?}"
        );
        let (_typed, type_diags) = typecheck(file.unwrap(), &schema);
        assert!(
            type_diags.iter().any(|d| d.code == *code),
            "expected {code} for {query}, got {type_diags:?}"
        );
    }
}

fn rewrite_aliases(sql: &str, numbered: bool) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut token_map: HashMap<String, String> = HashMap::new();
    let mut base_count: HashMap<String, usize> = HashMap::new();
    let mut chars = sql.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '$' && chars.peek() == Some(&'$') {
            chars.next();
            let mut base = String::new();
            while let Some(&n) = chars.peek() {
                if n.is_ascii_alphabetic() || n == '_' {
                    base.push(n);
                    chars.next();
                } else {
                    break;
                }
            }
            let mut digits = String::new();
            while let Some(&n) = chars.peek() {
                if n.is_ascii_digit() {
                    digits.push(n);
                    chars.next();
                } else {
                    break;
                }
            }
            if digits.is_empty() {
                out.push_str("$$");
                out.push_str(&base);
            } else if numbered {
                let token = format!("$${base}{digits}");
                if !token_map.contains_key(&token) {
                    let k = base_count.entry(base.clone()).or_insert(0);
                    token_map.insert(token.clone(), format!("$${base}#{}", *k));
                    *k += 1;
                }
                out.push_str(&token_map[&token]);
            } else {
                out.push_str("$$#");
            }
            continue;
        }
        out.push(c);
    }
    out
}

fn assert_sql_eq(actual: &str, expected: &str) {
    let (a, e) = (
        rewrite_aliases(actual, true),
        rewrite_aliases(expected, true),
    );
    assert_eq!(
        a, e,
        "SQL mismatch after alias normalization\n  actual:   {actual}\n  expected: {expected}"
    );
}

fn assert_sql_contains(actual: &str, fragment: &str) {
    let (a, f) = (
        rewrite_aliases(actual, false),
        rewrite_aliases(fragment, false),
    );
    assert!(
        a.contains(&f),
        "SQL missing fragment after alias normalization\n  actual:   {actual}\n  fragment: {fragment}"
    );
}

#[test]
fn scalar_int_add() {
    let (sql, params) = sql("1 + 2");
    assert_sql_eq(&sql, r#"SELECT "$$add"(?, ?) AS "$$val0""#);
    assert_eq!(params, vec![int_param(1), int_param(2)]);
}

#[test]
fn scalar_string_contains() {
    let (sql, params) = sql(r#""abc".contains("b")"#);
    assert_sql_eq(&sql, r#"SELECT ("instr"(?, ?) > ?) AS "$$val0""#);
    assert_eq!(
        params,
        vec![
            Param::Value(ParamValue::String("abc".to_string())),
            Param::Value(ParamValue::String("b".to_string())),
            int_param(0),
        ]
    );
}

#[test]
fn scalar_if_case() {
    let (sql, params) = sql("if true { 1 } else { 2 }");
    assert_sql_eq(
        &sql,
        r#"SELECT (CASE WHEN ? THEN ? ELSE ? END) AS "$$val0""#,
    );
    assert_eq!(
        params,
        vec![
            Param::Value(ParamValue::Bool(true)),
            int_param(1),
            int_param(2),
        ]
    );
}

#[test]
fn scalar_cast_float() {
    let (sql, params) = sql("(1 as Float)");
    assert_sql_eq(&sql, r#"SELECT "$$cast"(?, ?) AS "$$val0""#);
    assert_eq!(
        params,
        vec![
            int_param(1),
            Param::Value(ParamValue::String("float".to_string())),
        ]
    );
}

#[test]
fn scalar_instant_year() {
    let (sql, params) = sql("@2026-01-15.year()");
    assert_sql_eq(&sql, r#"SELECT "$$instant_year"(?) AS "$$val0""#);
    assert_eq!(
        params,
        vec![Param::Value(ParamValue::Instant(
            chrono::NaiveDate::from_ymd_opt(2026, 1, 15)
                .unwrap()
                .and_hms_opt(0, 0, 0)
                .unwrap()
                .and_utc()
        ))]
    );
}

#[test]
fn scalar_instant_to_rfc3339() {
    let (sql, params) = sql("@2026-01-15.to_rfc3339()");
    assert_sql_eq(&sql, r#"SELECT "$$instant_to_rfc3339"(?) AS "$$val0""#);
    assert_eq!(
        params,
        vec![Param::Value(ParamValue::Instant(
            chrono::NaiveDate::from_ymd_opt(2026, 1, 15)
                .unwrap()
                .and_hms_opt(0, 0, 0)
                .unwrap()
                .and_utc()
        ))]
    );
}

#[test]
fn scalar_float_round_floor_ceil() {
    let (sql_round, params_round) = sql("1.5f.round()");
    assert_sql_eq(&sql_round, r#"SELECT "$$float_round"(?) AS "$$val0""#);
    assert_eq!(params_round, vec![Param::Value(ParamValue::Float(1.5))]);

    let (sql_floor, _) = sql("1.5f.floor()");
    assert_sql_contains(&sql_floor, r#""$$float_floor"(?)"#);

    let (sql_ceil, _) = sql("1.5f.ceil()");
    assert_sql_contains(&sql_ceil, r#""$$float_ceil"(?)"#);
}

#[test]
fn scalar_int_min_constant() {
    let (sql, params) = sql("Int::MIN");
    assert_sql_eq(&sql, r#"SELECT ? AS "$$val0""#);
    assert_eq!(params, vec![int_param(i64::MIN)]);
}

#[test]
fn scalar_some_transparent() {
    let (sql, params) = sql("some(1)");
    assert_sql_eq(&sql, r#"SELECT ? AS "$$val0""#);
    assert_eq!(params, vec![int_param(1)]);
}

#[test]
fn scalar_duration_neg() {
    let (sql, params) = sql("-#1d");
    assert_sql_eq(&sql, r#"SELECT "$$neg"(?, ?) AS "$$val0""#);
    assert_eq!(
        params,
        vec![
            Param::Value(ParamValue::Duration(
                chrono::TimeDelta::try_days(1).unwrap()
            )),
            Param::Value(ParamValue::String("duration".to_string())),
        ]
    );
}

#[test]
fn scalar_tuple_ordering() {
    let (sql1, params1) = sql("(1, 2) < (3, 4)");
    assert_sql_eq(
        &sql1,
        r#"SELECT ((? < ?) OR ((? IS ?) AND (? < ?))) AS "$$val0""#,
    );
    assert_eq!(
        params1,
        vec![
            int_param(1),
            int_param(3),
            int_param(1),
            int_param(3),
            int_param(2),
            int_param(4),
        ]
    );

    let (sql2, params2) = sql("(1, 2) < (1, 3)");
    assert_sql_eq(
        &sql2,
        r#"SELECT ((? < ?) OR ((? IS ?) AND (? < ?))) AS "$$val0""#,
    );
    assert_eq!(
        params2,
        vec![
            int_param(1),
            int_param(1),
            int_param(1),
            int_param(1),
            int_param(2),
            int_param(3),
        ]
    );

    let (sql3, params3) = sql("(1, 2) <= (1, 2)");
    assert_sql_eq(
        &sql3,
        r#"SELECT (((? < ?) OR ((? IS ?) AND (? < ?))) OR ((? IS ?) AND (? IS ?))) AS "$$val0""#,
    );
    assert_eq!(
        params3,
        vec![
            int_param(1),
            int_param(1),
            int_param(1),
            int_param(1),
            int_param(2),
            int_param(2),
            int_param(1),
            int_param(1),
            int_param(2),
            int_param(2),
        ]
    );
    let (sql4, params4) = sql("(3, 4) > (1, 9)");
    assert_sql_eq(
        &sql4,
        r#"SELECT ((? < ?) OR ((? IS ?) AND (? < ?))) AS "$$val0""#,
    );
    assert_eq!(
        params4,
        vec![
            int_param(1),
            int_param(3),
            int_param(1),
            int_param(3),
            int_param(9),
            int_param(4),
        ]
    );

    let (sql5, params5) = sql("users[pos < (3.0f, 4.0f)] { name }");
    assert_sql_eq(
        &sql5,
        r#"SELECT "$$users0"."name" FROM "users" AS "$$users0" WHERE (("$$users0"."lat" < ?) OR (("$$users0"."lat" IS ?) AND ("$$users0"."lng" < ?)))"#,
    );
    assert_eq!(
        params5,
        vec![
            Param::Value(ParamValue::Float(3.0)),
            Param::Value(ParamValue::Float(3.0)),
            Param::Value(ParamValue::Float(4.0)),
        ]
    );
}

#[test]
fn scalar_tuple_equality() {
    let (sql1, params1) = sql("(1, 2) == (1, 2)");
    assert_sql_eq(&sql1, r#"SELECT ((? = ?) AND (? = ?)) AS "$$val0""#);
    assert_eq!(
        params1,
        vec![int_param(1), int_param(1), int_param(2), int_param(2)]
    );

    let (sql2, params2) = sql("(1, 2) != (1, 3)");
    assert_sql_eq(&sql2, r#"SELECT NOT ((? = ?) AND (? = ?)) AS "$$val0""#);
    assert_eq!(
        params2,
        vec![int_param(1), int_param(1), int_param(2), int_param(3)]
    );

    let (sql3, params3) = sql("users[(age, 1) == (30, 1)] { name }");
    assert_sql_eq(
        &sql3,
        r#"SELECT "$$users0"."name" FROM "users" AS "$$users0" WHERE (("$$users0"."age" = ?) AND (? = ?))"#,
    );
    assert_eq!(params3, vec![int_param(30), int_param(1), int_param(1)]);

    let (sql4, params4) = sql("users[pos == (1.0f, 2.0f)] { name }");
    assert_sql_eq(
        &sql4,
        r#"SELECT "$$users0"."name" FROM "users" AS "$$users0" WHERE (("$$users0"."lat" = ?) AND ("$$users0"."lng" = ?))"#,
    );
    assert_eq!(
        params4,
        vec![
            Param::Value(ParamValue::Float(1.0)),
            Param::Value(ParamValue::Float(2.0)),
        ]
    );

    let (sql5, params5) = sql("users[(nickname, age) == (nickname, age)] { name }");
    assert_sql_eq(
        &sql5,
        r#"SELECT "$$users0"."name" FROM "users" AS "$$users0" WHERE (("$$users0"."nickname" IS "$$users0"."nickname") AND ("$$users0"."age" = "$$users0"."age"))"#,
    );
    assert!(params5.is_empty());

    let (sql6, params6) = sql("(if true { (1, 2) } else { (3, 4) }) == (1, 2)");
    assert_sql_eq(
        &sql6,
        r#"SELECT (((CASE WHEN ? THEN ? ELSE ? END) = ?) AND ((CASE WHEN ? THEN ? ELSE ? END) = ?)) AS "$$val0""#,
    );
    assert_eq!(
        params6,
        vec![
            Param::Value(ParamValue::Bool(true)),
            int_param(1),
            int_param(3),
            int_param(1),
            Param::Value(ParamValue::Bool(true)),
            int_param(2),
            int_param(4),
            int_param(2),
        ]
    );

    let (sql6, params6) = sql("users { t = (age, 1) }[t == (30, 1)] { t }");
    assert_sql_eq(
        &sql6,
        r#"SELECT "$$subq0"."t_c0", "$$subq0"."t_c1" FROM (SELECT "$$users0"."age" AS "t_c0", ? AS "t_c1" FROM "users" AS "$$users0") AS "$$subq0" WHERE (("$$subq0"."t_c0" = ?) AND ("$$subq0"."t_c1" = ?))"#,
    );
    assert_eq!(params6, vec![int_param(1), int_param(30), int_param(1)]);
}

#[test]
fn scalar_tuple_in() {
    let (sql1, params1) = sql("(1, 2) in ((1, 2), (3, 4))");
    assert_sql_eq(
        &sql1,
        r#"SELECT (((? = ?) AND (? = ?)) OR ((? = ?) AND (? = ?))) AS "$$val0""#,
    );
    assert_eq!(
        params1,
        vec![
            int_param(1),
            int_param(1),
            int_param(2),
            int_param(2),
            int_param(1),
            int_param(3),
            int_param(2),
            int_param(4),
        ]
    );

    let (sql2, params2) = sql("(5, 6) in ((1, 2), (3, 4))");
    assert_sql_eq(
        &sql2,
        r#"SELECT (((? = ?) AND (? = ?)) OR ((? = ?) AND (? = ?))) AS "$$val0""#,
    );
    assert_eq!(
        params2,
        vec![
            int_param(5),
            int_param(1),
            int_param(6),
            int_param(2),
            int_param(5),
            int_param(3),
            int_param(6),
            int_param(4),
        ]
    );
}

#[test]
fn take_over_projection_merges() {
    let (guard_sql, guard_params) = case_pos_guard(2, "take");
    let (sql, params) = sql("users { name }.take(2)");
    assert_sql_eq(
        &sql,
        &format!(r#"SELECT "$$users0"."name" FROM "users" AS "$$users0" LIMIT ({guard_sql})"#),
    );
    assert_eq!(params, guard_params);
}

#[test]
fn filter_after_take_wraps() {
    let (guard_sql, mut expected) = case_pos_guard(2, "take");
    expected.push(int_param(30));
    let (sql, params) = sql("users.take(2)[age > 30] { name }");
    assert_sql_eq(
        &sql,
        &format!(
            r#"SELECT "$$subq0"."name" FROM (SELECT "$$users1".* FROM "users" AS "$$users1" LIMIT ({guard_sql})) AS "$$subq0" WHERE ("$$subq0"."age" > ?)"#
        ),
    );
    assert_eq!(params, expected);
}

#[test]
fn double_sort_last_wins() {
    let (sql, params) = sql("users.sort_asc(age).sort_desc(name) { name }");
    assert_sql_eq(
        &sql,
        r#"SELECT "$$users0"."name" FROM "users" AS "$$users0" ORDER BY "$$users0"."name" DESC"#,
    );
    assert!(params.is_empty());
}

#[test]
fn flat_projection() {
    let (sql, params) = sql("users { name, age }");
    assert_sql_eq(
        &sql,
        r#"SELECT "$$users0"."name", "$$users0"."age" FROM "users" AS "$$users0""#,
    );
    assert!(params.is_empty());
}

#[test]
fn renamed_column_alias() {
    let (sql, params) = sql("users { email }");
    assert_sql_eq(
        &sql,
        r#"SELECT "$$users0"."address" AS "email" FROM "users" AS "$$users0""#,
    );
    assert!(params.is_empty());
}

#[test]
fn filter_arg_compose() {
    let (sql, params) = sql("users[active][age >= 18] { name }");
    assert_sql_eq(
        &sql,
        r#"SELECT "$$users0"."name" FROM "users" AS "$$users0" WHERE ("$$users0"."age" >= ?) AND "$$users0"."active""#,
    );
    assert_eq!(params, vec![int_param(18)]);
}

#[test]
fn equality_non_optional() {
    let (sql, params) = sql("users[age == 18] { name }");
    assert_sql_eq(
        &sql,
        r#"SELECT "$$users0"."name" FROM "users" AS "$$users0" WHERE ("$$users0"."age" = ?)"#,
    );
    assert_eq!(params, vec![int_param(18)]);
}

#[test]
fn equality_optional_null_tolerant() {
    let (sql_eq, params) = sql("users[nickname == none] { name }");
    assert_sql_eq(
        &sql_eq,
        r#"SELECT "$$users0"."name" FROM "users" AS "$$users0" WHERE ("$$users0"."nickname" IS ?)"#,
    );
    assert_eq!(params, vec![Param::Value(ParamValue::Null)]);

    let (sql_ne, _) = sql("users[nickname != nickname] { name }");
    assert_sql_eq(
        &sql_ne,
        r#"SELECT "$$users0"."name" FROM "users" AS "$$users0" WHERE ("$$users0"."nickname" IS NOT "$$users0"."nickname")"#,
    );
}

#[test]
fn bool_operations_precedence() {
    let (sql, params) = sql("users[!active || age < 30] { name }");
    assert_sql_eq(
        &sql,
        r#"SELECT "$$users0"."name" FROM "users" AS "$$users0" WHERE (NOT "$$users0"."active" OR ("$$users0"."age" < ?))"#,
    );
    assert_eq!(params, vec![int_param(30)]);
}

#[test]
fn conditional_if_else() {
    let (sql, params) = sql("users[if active { age } else { 0 } > 18] { name }");
    assert_sql_eq(
        &sql,
        r#"SELECT "$$users0"."name" FROM "users" AS "$$users0" WHERE ((CASE WHEN "$$users0"."active" THEN "$$users0"."age" ELSE ? END) > ?)"#,
    );
    assert_eq!(params, vec![int_param(0), int_param(18)]);
}

#[test]
fn conditional_else_if_chain() {
    let (sql, params) =
        sql(r#"users[if age > 30 { "a" } else if age > 20 { "b" } else { "c" } == "a"] { name }"#);
    assert_sql_eq(
        &sql,
        r#"SELECT "$$users0"."name" FROM "users" AS "$$users0" WHERE ((CASE WHEN "$$users0"."age" > ? THEN ? WHEN "$$users0"."age" > ? THEN ? ELSE ? END) = ?)"#,
    );
    assert_eq!(
        params,
        vec![
            int_param(30),
            Param::Value(ParamValue::String("a".to_string())),
            int_param(20),
            Param::Value(ParamValue::String("b".to_string())),
            Param::Value(ParamValue::String("c".to_string())),
            Param::Value(ParamValue::String("a".to_string())),
        ]
    );
}

#[test]
fn record_deep_equality() {
    {
        let (sql1, params1) = sql("users[manager == manager] { name }");
        assert_sql_eq(
            &sql1,
            r#"SELECT "$$users0"."name" FROM "users" AS "$$users0" WHERE ("$$users0"."manager_id" = "$$users0"."manager_id")"#,
        );
        assert!(params1.is_empty());
    }
    {
        let (sql1, params1) = sql("users[mentor == mentor] { name }");
        assert_sql_eq(
            &sql1,
            r#"SELECT "$$users0"."name" FROM "users" AS "$$users0" WHERE ("$$users0"."mentor_id" IS "$$users0"."mentor_id")"#,
        );
        assert!(params1.is_empty());
    }
    {
        let (sql1, params1) = sql("users[some(manager) == some(manager)] { name }");
        assert_sql_eq(
            &sql1,
            r#"SELECT "$$users0"."name" FROM "users" AS "$$users0" WHERE ("$$users0"."manager_id" IS "$$users0"."manager_id")"#,
        );
        assert!(params1.is_empty());
    }
    {
        let (sql1, params1) = sql("{ n = 1 } == { n = 1 }");
        assert_sql_eq(&sql1, r#"SELECT (? = ?) AS "$$val0""#);
        assert_eq!(params1, vec![int_param(1), int_param(1)]);
    }
    {
        let (sql1, params1) = sql(r#"users[manager == { name = "Bob" }] { name }"#);
        assert_sql_eq(
            &sql1,
            r#"SELECT "$$users0"."name" FROM "users" AS "$$users0" WHERE ((SELECT "$$users1"."name" FROM "users" AS "$$users1" WHERE ("$$users1"."id" = "$$users0"."manager_id")) = ?)"#,
        );
        assert_eq!(params1, vec![str_param("Bob")]);
    }
}

#[test]
fn record_spine_equality() {
    {
        let (sql1, params1) = sql("users[manager.manager == manager] { name }");
        assert_sql_eq(
            &sql1,
            r#"SELECT "$$users0"."name" FROM "users" AS "$$users0" WHERE ((SELECT "$$users1"."manager_id" FROM "users" AS "$$users1" WHERE ("$$users1"."id" = "$$users0"."manager_id")) IS "$$users0"."manager_id")"#,
        );
        assert!(params1.is_empty());
    }
    {
        let (sql1, params1) = sql("users[mentor?.manager == mentor] { name }");
        assert_sql_eq(
            &sql1,
            r#"SELECT "$$users0"."name" FROM "users" AS "$$users0" WHERE ((SELECT "$$users1"."manager_id" FROM "users" AS "$$users1" WHERE ("$$users1"."id" = "$$users0"."mentor_id")) IS "$$users0"."mentor_id")"#,
        );
        assert!(params1.is_empty());
    }
    {
        let (sql1, params1) = sql(r#"users[manager.manager == { name = "Bob" }] { name }"#);
        assert_sql_eq(
            &sql1,
            r#"SELECT "$$users0"."name" FROM "users" AS "$$users0" WHERE ((SELECT (SELECT "$$users3"."name" FROM "users" AS "$$users3" WHERE ("$$users3"."id" = "$$users2"."manager_id")) AS "$$val4" FROM "users" AS "$$users2" WHERE ("$$users2"."id" = "$$users0"."manager_id")) = ?)"#,
        );
        assert_eq!(params1, vec![str_param("Bob")]);
    }
    {
        let (sql1, params1) = sql("users[settings == settings] { name }");
        assert_sql_eq(
            &sql1,
            r#"SELECT "$$users0"."name" FROM "users" AS "$$users0" WHERE ?"#,
        );
        assert_eq!(params1, vec![Param::Value(ParamValue::Bool(true))]);
    }
}

#[test]
fn list_deep_equality() {
    {
        let (sql1, params1) = sql("[1, 2] == [1, 2]");
        assert_sql_eq(
            &sql1,
            r#"SELECT (((? = ?) AND (? = ?)) AND (? = ?)) AS "$$val0""#,
        );
        assert_eq!(
            params1,
            vec![
                int_param(2),
                int_param(2),
                int_param(1),
                int_param(1),
                int_param(2),
                int_param(2),
            ]
        );
    }
    {
        let (sql1, params1) = sql("[1, 2] == [1, 2, 3]");
        assert_sql_eq(
            &sql1,
            r#"SELECT (((? = ?) AND (? = ?)) AND (? = ?)) AS "$$val0""#,
        );
        assert_eq!(
            params1,
            vec![
                int_param(2),
                int_param(3),
                int_param(1),
                int_param(1),
                int_param(2),
                int_param(2),
            ]
        );
    }
    {
        let (sql1, params1) = sql("[1, 2] == [2, 1]");
        assert_sql_eq(
            &sql1,
            r#"SELECT (((? = ?) AND (? = ?)) AND (? = ?)) AS "$$val0""#,
        );
        assert_eq!(
            params1,
            vec![
                int_param(2),
                int_param(2),
                int_param(1),
                int_param(2),
                int_param(2),
                int_param(1),
            ]
        );
    }
    {
        let (sql1, params1) = sql("[(1, 2)] == [(1, 2)]");
        assert_sql_eq(
            &sql1,
            r#"SELECT ((? = ?) AND ((? = ?) AND (? = ?))) AS "$$val0""#,
        );
        assert_eq!(
            params1,
            vec![
                int_param(1),
                int_param(1),
                int_param(1),
                int_param(1),
                int_param(2),
                int_param(2),
            ]
        );
    }
    {
        let (sql1, params1) = sql("users[tags == tags] { name }");
        assert_sql_eq(
            &sql1,
            r#"SELECT "$$users0"."name" FROM "users" AS "$$users0" WHERE (((SELECT COUNT(*) AS "$$val7" FROM (SELECT "$$subq3"."user_key", "$$subq3"."tag_value", "ROW_NUMBER"() OVER () AS "$$val2" FROM (SELECT "$$user_tags1".* FROM "user_tags" AS "$$user_tags1" WHERE ("$$user_tags1"."user_key" = "$$users0"."id")) AS "$$subq3") AS "$$subq8") = (SELECT COUNT(*) AS "$$val9" FROM (SELECT "$$subq6"."user_key", "$$subq6"."tag_value", "ROW_NUMBER"() OVER () AS "$$val5" FROM (SELECT "$$user_tags4".* FROM "user_tags" AS "$$user_tags4" WHERE ("$$user_tags4"."user_key" = "$$users0"."id")) AS "$$subq6") AS "$$subq10")) AND NOT EXISTS (SELECT "$$subq12".* FROM (SELECT "$$subq3"."user_key", "$$subq3"."tag_value", "ROW_NUMBER"() OVER () AS "$$val2" FROM (SELECT "$$user_tags1".* FROM "user_tags" AS "$$user_tags1" WHERE ("$$user_tags1"."user_key" = "$$users0"."id")) AS "$$subq3") AS "$$subq12" WHERE NOT EXISTS (SELECT "$$subq11".* FROM (SELECT "$$subq6"."user_key", "$$subq6"."tag_value", "ROW_NUMBER"() OVER () AS "$$val5" FROM (SELECT "$$user_tags4".* FROM "user_tags" AS "$$user_tags4" WHERE ("$$user_tags4"."user_key" = "$$users0"."id")) AS "$$subq6") AS "$$subq11" WHERE ("$$subq11"."$$val5" = "$$subq12"."$$val2") AND ("$$subq11"."user_key" IS "$$subq12"."user_key") AND ("$$subq11"."tag_value" IS "$$subq12"."tag_value"))))"#,
        );
        assert!(params1.is_empty());
    }
    {
        let (sql1, params1) = sql("(if true { [1] } else { [2] }) == [1]");
        assert_sql_contains(&sql1, "COUNT(*)");
        assert_sql_contains(&sql1, "ROW_NUMBER");
        assert_sql_contains(&sql1, "NOT EXISTS");
        assert_eq!(params1, vec![int_param(1), int_param(1),]);
    }
}

#[test]
fn composite_contains() {
    {
        let (sql1, params1) = sql("[(1, 2)].contains((1, 2))");
        assert_sql_eq(&sql1, r#"SELECT ((? = ?) AND (? = ?)) AS "$$val0""#);
        assert_eq!(
            params1,
            vec![int_param(1), int_param(1), int_param(2), int_param(2)]
        );
    }
    {
        let (sql1, params1) = sql("[(1, 2), (3, 4)].contains((1, 2))");
        assert_sql_eq(
            &sql1,
            r#"SELECT (((? = ?) AND (? = ?)) OR ((? = ?) AND (? = ?))) AS "$$val0""#,
        );
        assert_eq!(
            params1,
            vec![
                int_param(1),
                int_param(1),
                int_param(2),
                int_param(2),
                int_param(3),
                int_param(1),
                int_param(4),
                int_param(2),
            ]
        );
    }
    {
        let (sql1, params1) = sql("[{ a = 1 }].contains({ a = 1 })");
        assert_sql_eq(&sql1, r#"SELECT (? = ?) AS "$$val0""#);
        assert_eq!(params1, vec![int_param(1), int_param(1)]);
    }
    {
        let (sql1, params1) = sql("[(1, 2)].take(1).contains((1, 2))");
        assert_sql_eq(
            &sql1,
            r#"SELECT EXISTS (SELECT "lit1".* FROM "$$lit_2_II"('lit1') AS "lit1" WHERE ("lit1"."c0" IS ?) AND ("lit1"."c1" IS ?) LIMIT (CASE WHEN ? >= ? THEN ? ELSE "$$panic"(?) END)) AS "$$val0""#,
        );
        assert_eq!(
            params1,
            vec![
                int_param(1),
                int_param(2),
                int_param(1),
                int_param(0),
                int_param(1),
                limit_msg("take"),
            ]
        );
    }
    {
        let (sql1, params1) = sql("users[orders.contains({ total = 10.0 })] { name }");
        assert_sql_eq(
            &sql1,
            r#"SELECT "$$users0"."name" FROM "users" AS "$$users0" WHERE EXISTS (SELECT "$$orders1".* FROM "orders" AS "$$orders1" WHERE ("$$orders1"."user_id" = "$$users0"."id") AND ("$$orders1"."total" IS ?))"#,
        );
        assert_eq!(
            params1,
            vec![Param::Value(ParamValue::Dec(rust_decimal::Decimal::new(
                100, 1
            )))]
        );
    }
}

#[test]
fn list_mixed_equality() {
    {
        let (sql1, params1) = sql(r#"users[tags == ["x", "y"]] { name }"#);
        assert_sql_eq(
            &sql1,
            r#"SELECT "$$users0"."name" FROM "users" AS "$$users0" WHERE (((SELECT COUNT(*) AS "$$val6" FROM (SELECT "$$subq3"."tag_value", "ROW_NUMBER"() OVER () AS "$$val2" FROM (SELECT "$$user_tags1".* FROM "user_tags" AS "$$user_tags1" WHERE ("$$user_tags1"."user_key" = "$$users0"."id")) AS "$$subq3") AS "$$subq7") = (SELECT COUNT(*) AS "$$val8" FROM (SELECT "$$subq5"."c0", "ROW_NUMBER"() OVER () AS "$$val4" FROM (SELECT ? AS "c0" UNION ALL SELECT ? AS "c0") AS "$$subq5") AS "$$subq9")) AND NOT EXISTS (SELECT "$$subq11".* FROM (SELECT "$$subq3"."tag_value", "ROW_NUMBER"() OVER () AS "$$val2" FROM (SELECT "$$user_tags1".* FROM "user_tags" AS "$$user_tags1" WHERE ("$$user_tags1"."user_key" = "$$users0"."id")) AS "$$subq3") AS "$$subq11" WHERE NOT EXISTS (SELECT "$$subq10".* FROM (SELECT "$$subq5"."c0", "ROW_NUMBER"() OVER () AS "$$val4" FROM (SELECT ? AS "c0" UNION ALL SELECT ? AS "c0") AS "$$subq5") AS "$$subq10" WHERE ("$$subq10"."$$val4" = "$$subq11"."$$val2") AND ("$$subq10"."c0" IS "$$subq11"."tag_value"))))"#,
        );
        assert_eq!(
            params1,
            vec![
                str_param("x"),
                str_param("y"),
                str_param("x"),
                str_param("y")
            ]
        );
    }
    {
        let (sql1, params1) = sql(r#"users[["z"] == tags] { name }"#);
        assert_sql_contains(&sql1, "ROW_NUMBER");
        assert_sql_contains(&sql1, "NOT EXISTS");
        assert_eq!(params1, vec![str_param("z"), str_param("z")]);
    }
}

#[test]
fn membership_in_tuple() {
    let (sql_int, params_int) = sql("users[age in (18, 21)] { name }");
    assert_sql_eq(
        &sql_int,
        r#"SELECT "$$users0"."name" FROM "users" AS "$$users0" WHERE ("$$users0"."age" IN (?, ?))"#,
    );
    assert_eq!(params_int, vec![int_param(18), int_param(21)]);

    let (sql_str, params_str) = sql(r#"users[name in ("a", "b")] { name }"#);
    assert_sql_eq(
        &sql_str,
        r#"SELECT "$$users0"."name" FROM "users" AS "$$users0" WHERE ("$$users0"."name" IN (?, ?))"#,
    );
    assert_eq!(
        params_str,
        vec![
            Param::Value(ParamValue::String("a".to_string())),
            Param::Value(ParamValue::String("b".to_string())),
        ]
    );
}

#[test]
fn string_contains() {
    let (sql, params) = sql(r#"users[name.contains("ob")] { name }"#);
    assert_sql_eq(
        &sql,
        r#"SELECT "$$users0"."name" FROM "users" AS "$$users0" WHERE ("instr"("$$users0"."name", ?) > ?)"#,
    );
    assert_eq!(
        params,
        vec![
            Param::Value(ParamValue::String("ob".to_string())),
            int_param(0),
        ]
    );
}

#[test]
fn string_starts_ends_with() {
    let (sql_sw, params_sw) = sql(r#"users[email.starts_with("a@")] { name }"#);
    assert_sql_eq(
        &sql_sw,
        r#"SELECT "$$users0"."name" FROM "users" AS "$$users0" WHERE ("instr"("$$users0"."address", ?) = ?)"#,
    );
    assert_eq!(
        params_sw,
        vec![
            Param::Value(ParamValue::String("a@".to_string())),
            int_param(1),
        ]
    );

    let (sql2, params2) = sql(r#"users[email.ends_with(".com")] { name }"#);
    assert_sql_eq(
        &sql2,
        r#"SELECT "$$users0"."name" FROM "users" AS "$$users0" WHERE ("substr"("$$users0"."address", "$$add"("$$sub"("length"("$$users0"."address"), "length"(?)), ?)) = ?)"#,
    );
    assert_eq!(
        params2,
        vec![
            Param::Value(ParamValue::String(".com".to_string())),
            int_param(1),
            Param::Value(ParamValue::String(".com".to_string())),
        ]
    );
}

#[test]
fn string_case_len_to_string() {
    let (sql_up, params_up) = sql(r#"users[name.to_upper() == "X"] { name }"#);
    assert_sql_eq(
        &sql_up,
        r#"SELECT "$$users0"."name" FROM "users" AS "$$users0" WHERE ("$$upper"("$$users0"."name") = ?)"#,
    );
    assert_eq!(
        params_up,
        vec![Param::Value(ParamValue::String("X".to_string()))]
    );

    let (sql_lower, _) = sql(r#"users[name.to_lower() == "x"] { name }"#);
    assert_sql_contains(&sql_lower, r#""$$lower"("$$users0"."name")"#);

    let (sql_len, params_len) = sql("users[name.len() > 2] { name }");
    assert_sql_eq(
        &sql_len,
        r#"SELECT "$$users0"."name" FROM "users" AS "$$users0" WHERE ("length"("$$users0"."name") > ?)"#,
    );
    assert_eq!(params_len, vec![int_param(2)]);

    let (sql_str, params_str) = sql(r#"users[age.to_string() == "30"] { name }"#);
    assert_sql_eq(
        &sql_str,
        r#"SELECT "$$users0"."name" FROM "users" AS "$$users0" WHERE (CAST("$$users0"."age" AS TEXT) = ?)"#,
    );
    assert_eq!(
        params_str,
        vec![Param::Value(ParamValue::String("30".to_string()))]
    );

    let (sql_dec, params_dec) = sql(r#"users[balance.to_string() == "0.5"] { name }"#);
    assert_sql_eq(
        &sql_dec,
        r#"SELECT "$$users0"."name" FROM "users" AS "$$users0" WHERE ("$$dec_to_string"("$$users0"."balance") = ?)"#,
    );
    assert_eq!(
        params_dec,
        vec![Param::Value(ParamValue::String("0.5".to_string()))]
    );
}

#[test]
fn arithmetic_int_add() {
    let (sql, params) = sql("users[age + 1 > 18] { name }");
    assert_sql_eq(
        &sql,
        r#"SELECT "$$users0"."name" FROM "users" AS "$$users0" WHERE ("$$add"("$$users0"."age", ?) > ?)"#,
    );
    assert_eq!(params, vec![int_param(1), int_param(18)]);
}

#[test]
fn arithmetic_float_mul() {
    let (sql, params) = sql("users[score * 2.0f > 1.0f] { name }");
    assert_sql_eq(
        &sql,
        r#"SELECT "$$users0"."name" FROM "users" AS "$$users0" WHERE ("$$mul"("$$users0"."score", ?) > ?)"#,
    );
    assert_eq!(
        params,
        vec![
            Param::Value(ParamValue::Float(2.0)),
            Param::Value(ParamValue::Float(1.0)),
        ]
    );
}

#[test]
fn arithmetic_dec_sub() {
    let (sql, params) = sql("users[balance - 0.5 > 0.0] { name }");
    assert_sql_eq(
        &sql,
        r#"SELECT "$$users0"."name" FROM "users" AS "$$users0" WHERE ("$$sub"("$$users0"."balance", ?) > ?)"#,
    );
    assert_eq!(
        params,
        vec![
            Param::Value(ParamValue::Dec(rust_decimal::Decimal::new(5, 1))),
            Param::Value(ParamValue::Dec(rust_decimal::Decimal::new(0, 0))),
        ]
    );
}

#[test]
fn arithmetic_int_div_rem_mod() {
    let (sql_div, _) = sql("users[age / 2 > 1] { name }");
    assert_sql_contains(&sql_div, r#""$$div"("$$users0"."age", ?)"#);
    let (sql_mod, _) = sql("users[age % 2 == 0] { name }");
    assert_sql_contains(&sql_mod, r#""$$rem"("$$users0"."age", ?)"#);
    let (sql_mod, _) = sql("users[age mod 2 == 0] { name }");
    assert_sql_contains(&sql_mod, r#""$$mod"("$$users0"."age", ?)"#);
}

#[test]
fn arithmetic_duration_scalar() {
    let (sql, params) = sql("users[ttl * 2 > #1d] { name }");
    assert_sql_eq(
        &sql,
        r#"SELECT "$$users0"."name" FROM "users" AS "$$users0" WHERE ("$$mul"("$$users0"."ttl", ?, ?, ?) > ?)"#,
    );
    assert_eq!(
        params,
        vec![
            int_param(2),
            Param::Value(ParamValue::String("duration".to_string())),
            Param::Value(ParamValue::Null),
            Param::Value(ParamValue::Duration(
                chrono::TimeDelta::try_days(1).unwrap()
            )),
        ]
    );
}

#[test]
fn arithmetic_instant_duration() {
    let (sql, params) = sql("users[created + ttl > @2026-01-02] { name }");
    assert_sql_eq(
        &sql,
        r#"SELECT "$$users0"."name" FROM "users" AS "$$users0" WHERE ("$$add"("$$users0"."created", "$$users0"."ttl", ?, ?) > ?)"#,
    );
    assert_eq!(
        params,
        vec![
            Param::Value(ParamValue::String("instant".to_string())),
            Param::Value(ParamValue::String("duration".to_string())),
            Param::Value(ParamValue::Instant(
                chrono::DateTime::from_timestamp(1767312000, 0).unwrap()
            )),
        ]
    );
}

#[test]
fn arithmetic_instant_instant() {
    let (sql, _) = sql("users[created - @2026 > #0s] { name }");
    assert_sql_contains(&sql, r#""$$sub"("$$users0"."created", ?, ?, ?)"#);
}

#[test]
fn arithmetic_duration_neg() {
    let (sql, _) = sql("users[ttl > -ttl] { name }");
    assert_sql_contains(&sql, r#""$$neg"("$$users0"."ttl", ?)"#);
}

#[test]
fn cast_numeric() {
    let (sql_if, params_if) = sql("users[(age as Float) > 1.5f] { name }");
    assert_sql_eq(
        &sql_if,
        r#"SELECT "$$users0"."name" FROM "users" AS "$$users0" WHERE ("$$cast"("$$users0"."age", ?) > ?)"#,
    );
    assert_eq!(
        params_if,
        vec![
            Param::Value(ParamValue::String("float".to_string())),
            Param::Value(ParamValue::Float(1.5)),
        ]
    );

    let (sql_id, params_id) = sql("users[(age as Dec) > 0.0] { name }");
    assert_sql_contains(&sql_id, r#""$$cast"("$$users0"."age", ?)"#);
    assert_eq!(
        params_id,
        vec![
            Param::Value(ParamValue::String("dec".to_string())),
            Param::Value(ParamValue::Dec(rust_decimal::Decimal::new(0, 0))),
        ]
    );

    let (sql_fi, params_fi) = sql("users[(score as Int) > 1] { name }");
    assert_sql_contains(&sql_fi, r#""$$cast"("$$users0"."score", ?)"#);
    assert_eq!(
        params_fi,
        vec![
            Param::Value(ParamValue::String("int".to_string())),
            int_param(1),
        ]
    );

    let (sql_fd, params_fd) = sql("users[(score as Dec) > 0.0] { name }");
    assert_sql_contains(&sql_fd, r#""$$cast"("$$users0"."score", ?)"#);
    assert_eq!(
        params_fd[0],
        Param::Value(ParamValue::String("dec".to_string()))
    );

    let (sql_di, params_di) = sql("users[(balance as Int) > 0] { name }");
    assert_sql_contains(&sql_di, r#""$$cast"("$$users0"."balance", ?)"#);
    assert_eq!(
        params_di[0],
        Param::Value(ParamValue::String("int".to_string()))
    );

    let (sql_df, params_df) = sql("users[(balance as Float) > 1.0f] { name }");
    assert_sql_contains(&sql_df, r#""$$cast"("$$users0"."balance", ?)"#);
    assert_eq!(
        params_df[0],
        Param::Value(ParamValue::String("float".to_string()))
    );
}

#[test]
fn cast_bool_int_noop() {
    let (sql, params) = sql("users[(active as Int) == 1] { name }");
    assert_sql_eq(
        &sql,
        r#"SELECT "$$users0"."name" FROM "users" AS "$$users0" WHERE ("$$users0"."active" = ?)"#,
    );
    assert_eq!(params, vec![int_param(1)]);
}

#[test]
fn instant_duration_methods() {
    let (sql_y, params_y) = sql("users[created.year() == 2026] { name }");
    assert_sql_eq(
        &sql_y,
        r#"SELECT "$$users0"."name" FROM "users" AS "$$users0" WHERE ("$$instant_year"("$$users0"."created") = ?)"#,
    );
    assert_eq!(params_y, vec![int_param(2026)]);

    let (sql_dur, params_dur) = sql("users[ttl.as_hours() > 1.0] { name }");
    assert_sql_eq(
        &sql_dur,
        r#"SELECT "$$users0"."name" FROM "users" AS "$$users0" WHERE ("$$duration_as_hours"("$$users0"."ttl") > ?)"#,
    );
    assert_eq!(
        params_dur,
        vec![Param::Value(ParamValue::Dec(rust_decimal::Decimal::ONE))],
    );

    let (sql_ep, _) = sql("users[created.epoch() > 0.0] { name }");
    assert_sql_contains(&sql_ep, r#""$$instant_epoch"("$$users0"."created")"#);
}

#[test]
fn dec_round_dp() {
    let (sql, params) = sql("users[balance.round_dp(2) > 0.0] { name }");
    assert_sql_eq(
        &sql,
        r#"SELECT "$$users0"."name" FROM "users" AS "$$users0" WHERE ("$$dec_round_dp"("$$users0"."balance", ?) > ?)"#,
    );
    assert_eq!(
        params,
        vec![
            int_param(2),
            Param::Value(ParamValue::Dec(rust_decimal::Decimal::new(0, 0))),
        ]
    );
}

#[test]
fn wall_clock_constructors() {
    let (sql_now, params_now) = sql("users[created > @now] { name }");
    assert_sql_eq(
        &sql_now,
        r#"SELECT "$$users0"."name" FROM "users" AS "$$users0" WHERE ("$$users0"."created" > "$$now"())"#,
    );
    assert!(params_now.is_empty());

    let (sql_today, params_today) = sql("users[created > @today] { name }");
    assert_sql_eq(
        &sql_today,
        r#"SELECT "$$users0"."name" FROM "users" AS "$$users0" WHERE ("$$users0"."created" > "$$today"(?))"#,
    );
    assert_eq!(params_today, vec![Param::Value(ParamValue::Null)]);

    let (sql_time, params_time) = sql("users[created > @today_14:30] { name }");
    assert_sql_eq(
        &sql_time,
        r#"SELECT "$$users0"."name" FROM "users" AS "$$users0" WHERE ("$$users0"."created" > "$$today"(?))"#,
    );
    assert_eq!(
        params_time,
        vec![Param::Value(ParamValue::Duration(
            chrono::TimeDelta::try_minutes(870).unwrap()
        ))]
    );
}

#[test]
fn sort_take() {
    let (limit_sql, limit_params) = case_pos_guard(10, "take");
    let (sql, params) = sql("users.sort_desc(age).take(10) { name }");
    assert_sql_eq(
        &sql,
        &format!(
            r#"SELECT "$$users0"."name" FROM "users" AS "$$users0" ORDER BY "$$users0"."age" DESC LIMIT ({limit_sql})"#
        ),
    );
    assert_eq!(params, limit_params);
}

#[test]
fn sort_per_arg_direction() {
    let (sql, params) = sql("users.sort_desc(age, asc name) { name }");
    assert_sql_eq(
        &sql,
        r#"SELECT "$$users0"."name" FROM "users" AS "$$users0" ORDER BY "$$users0"."age" DESC, "$$users0"."name" ASC"#,
    );
    assert!(params.is_empty());
}

#[test]
fn sort_tuple_fields() {
    let (sql, params) = sql("users.sort_asc(pos) { name }");
    assert_sql_eq(
        &sql,
        r#"SELECT "$$users0"."name" FROM "users" AS "$$users0" ORDER BY "$$users0"."lat" ASC, "$$users0"."lng" ASC"#,
    );
    assert!(params.is_empty());
}

#[test]
fn skip_take_compose() {
    let (take_sql, mut expected) = case_pos_guard(20, "take");
    let (skip_sql, offset_params) = case_pos_guard(10, "skip");
    expected.extend(offset_params);
    let (sql, params) = sql("users.skip(10).take(20) { name }");
    assert_sql_eq(
        &sql,
        &format!(
            r#"SELECT "$$users0"."name" FROM "users" AS "$$users0" LIMIT ({take_sql}) OFFSET ({skip_sql})"#
        ),
    );
    assert_eq!(params, expected);
}

#[test]
fn take_skip_compose() {
    let (take_sql, mut expected) = case_pos_guard(5, "take");
    let (skip_sql, skip_params) = case_pos_guard(2, "skip");
    expected.extend(skip_params.clone());
    expected.push(int_param(0));
    expected.extend(skip_params);
    let (sql, params) = sql("users.take(5).skip(2) { name }");
    assert_sql_eq(
        &sql,
        &format!(
            r#"SELECT "$$users0"."name" FROM "users" AS "$$users0" LIMIT "max"("$$sub"({take_sql}, {skip_sql}), ?) OFFSET ({skip_sql})"#
        ),
    );
    assert_eq!(params, expected);
}

#[test]
fn skip_nth_compose() {
    let (nth_limit, mut expected) = (String::from("?"), vec![int_param(1)]);
    let (nth_offset, nth_params) = case_pos_guard(4, "nth");
    expected.extend(nth_params);
    let (skip_offset, skip_params) = case_pos_guard(3, "skip");
    expected.extend(skip_params);
    let (sql, params) = sql("users.skip(3).nth(4)");
    assert_sql_eq(
        &sql,
        &format!(
            r#"SELECT "$$users0".* FROM "users" AS "$$users0" LIMIT {nth_limit} OFFSET "$$add"({nth_offset}, {skip_offset})"#
        ),
    );
    assert_eq!(params, expected);
}

#[test]
fn take_nth_compose() {
    let mut expected = vec![int_param(1)];
    let (take_sql, take_params) = case_pos_guard(2, "take");
    expected.extend(take_params);
    let (nth_sql, nth_params) = case_pos_guard(4, "nth");
    expected.extend(nth_params.clone());
    expected.push(int_param(0));
    expected.extend(nth_params);

    let (sql, params) = sql("users.take(2).nth(4)");
    assert_sql_eq(
        &sql,
        &format!(
            r#"SELECT "$$users0".* FROM "users" AS "$$users0" LIMIT "min"(?, "max"("$$sub"({take_sql}, {nth_sql}), ?)) OFFSET ({nth_sql})"#
        ),
    );
    assert_eq!(params, expected);
}

#[test]
fn take_skip_compose_empty() {
    let (take_sql, mut expected) = case_pos_guard(1, "take");
    let (skip_sql, skip_params) = case_pos_guard(2, "skip");
    expected.extend(skip_params.clone());
    expected.push(int_param(0));
    expected.extend(skip_params);
    let (sql, params) = sql("users.take(1).skip(2) { name }");
    assert_sql_eq(
        &sql,
        &format!(
            r#"SELECT "$$users0"."name" FROM "users" AS "$$users0" LIMIT "max"("$$sub"({take_sql}, {skip_sql}), ?) OFFSET ({skip_sql})"#
        ),
    );
    assert_eq!(params, expected);
}

#[test]
fn take_take_compose() {
    let (first, mut expected) = case_pos_guard(5, "take");
    let (second, second_params) = case_pos_guard(30, "take");
    expected.extend(second_params);
    let (sql, params) = sql("users.take(30).take(5) { name }");
    assert_sql_eq(
        &sql,
        &format!(
            r#"SELECT "$$users0"."name" FROM "users" AS "$$users0" LIMIT "min"({first}, {second})"#
        ),
    );
    assert_eq!(params, expected);
}

#[test]
fn skip_skip_compose() {
    let (first, mut expected) = case_pos_guard(20, "skip");
    let (second, second_params) = case_pos_guard(10, "skip");
    expected.extend(second_params);
    let (sql, params) = sql("users.skip(10).skip(20) { name }");
    assert_sql_eq(
        &sql,
        &format!(
            r#"SELECT "$$users0"."name" FROM "users" AS "$$users0" LIMIT -1 OFFSET "$$add"({first}, {second})"#
        ),
    );
    assert_eq!(params, expected);
}

#[test]
fn take_first_compose() {
    let (take_sql, mut expected) = case_pos_guard(3, "take");
    expected.insert(0, int_param(1));
    let (sql, params) = sql("users.take(3).first()");
    assert_sql_eq(
        &sql,
        &format!(r#"SELECT "$$users0".* FROM "users" AS "$$users0" LIMIT "min"(?, {take_sql})"#),
    );
    assert_eq!(params, expected);
}

#[test]
fn take_sort_subquery_boundary() {
    let (inner_limit, mut expected) = case_pos_guard(20, "take");
    let (outer_limit, outer_params) = case_pos_guard(10, "take");
    expected.extend(outer_params);
    let (sql, params) = sql("users.sort_desc(age).take(20).sort_asc(name).take(10)");
    assert_sql_eq(
        &sql,
        &format!(
            r#"SELECT "$$subq0".* FROM (SELECT "$$users1".* FROM "users" AS "$$users1" ORDER BY "$$users1"."age" DESC LIMIT ({inner_limit})) AS "$$subq0" ORDER BY "$$subq0"."name" ASC LIMIT ({outer_limit})"#
        ),
    );
    assert_eq!(params, expected);
}

#[test]
fn filter_projection_subquery_boundary() {
    let (sql, params) = sql(r#"users { name }[name == "x"]"#);
    assert_sql_eq(
        &sql,
        r#"SELECT "$$subq0".* FROM (SELECT "$$users1"."name" FROM "users" AS "$$users1") AS "$$subq0" WHERE ("$$subq0"."name" = ?)"#,
    );
    assert_eq!(
        params,
        vec![Param::Value(ParamValue::String("x".to_string()))]
    );
}

#[test]
fn chained_projection_subquery_boundary() {
    let (sql, params) = sql("users { name } { name }");
    assert_sql_eq(
        &sql,
        r#"SELECT "$$subq0"."name" FROM (SELECT "$$users1"."name" FROM "users" AS "$$users1") AS "$$subq0""#,
    );
    assert!(params.is_empty());
}

#[test]
fn bare_literal_result() {
    let (sql, params) = sql("0");
    assert_sql_eq(&sql, r#"SELECT ? AS "$$val0""#);
    assert_eq!(params, vec![int_param(0)]);
}

#[test]
fn result_shape_output_names() {
    use crate::codegen::model::{Cardinality, ChildRef, ResultShape};
    let compiled = compile("users[active].sort_asc(name).take(5) { name, age }");
    assert_eq!(
        compiled.result,
        ResultShape::List {
            element: Box::new(ResultShape::Record(vec![
                ("name".to_string(), ResultShape::Column("name".to_string())),
                ("age".to_string(), ResultShape::Column("age".to_string())),
            ])),
            child: ChildRef {
                stmt: 0,
                parent: None,
                cardinality: Cardinality::Many,
                tag: None,
            },
        }
    );
    assert_eq!(compiled.statements[0].shape.columns.len(), 2);
}

#[test]
fn output_column_types() {
    use crate::codegen::model::{ColumnRole, OutputColumn};
    use grove_schema::validated::ScalarType;

    let col = |name: &str, role: ColumnRole, ty: Option<ScalarType>| OutputColumn {
        name: name.to_string(),
        role,
        ty,
    };

    let compiled = compile("users.take(1) { active }");
    assert_eq!(
        compiled.statements[0].shape.columns,
        vec![col("active", ColumnRole::Value, Some(ScalarType::Bool))]
    );

    let compiled = compile("1 + 2");
    assert_eq!(
        compiled.statements[0].shape.columns,
        vec![col("$$val0", ColumnRole::Value, Some(ScalarType::Int))]
    );

    let compiled = compile("[1, 2]");
    assert_eq!(
        compiled.statements[0].shape.columns,
        vec![col("c0", ColumnRole::Value, Some(ScalarType::Int))]
    );

    let compiled = compile("(1, 2.5)");
    assert_eq!(
        compiled.statements[0].shape.columns,
        vec![
            col("c0", ColumnRole::Value, Some(ScalarType::Int)),
            col("c1", ColumnRole::Value, Some(ScalarType::Dec)),
        ]
    );

    let compiled = compile("users.take(1) { score }");
    assert_eq!(
        compiled.statements[0].shape.columns,
        vec![col("score", ColumnRole::Value, Some(ScalarType::Float))]
    );
}

#[test]
fn nav_self_ref_filter() {
    let (sql, params) = sql(r#"users[manager.name == "Ada"] { name }"#);
    assert_sql_eq(
        &sql,
        r#"SELECT "$$users1"."name" FROM "users" AS "$$users1" WHERE ((SELECT "$$users0"."name" FROM "users" AS "$$users0" WHERE ("$$users0"."id" = "$$users1"."manager_id")) = ?)"#,
    );
    assert_eq!(params, vec![str_param("Ada")]);
}

#[test]
fn nav_self_ref_projection() {
    let (sql, params) = sql("users { boss = manager.name }");
    assert_sql_eq(
        &sql,
        r#"SELECT (SELECT "$$users0"."name" FROM "users" AS "$$users0" WHERE ("$$users0"."id" = "$$users1"."manager_id")) AS "boss" FROM "users" AS "$$users1""#,
    );
    assert!(params.is_empty());
}

#[test]
fn nav_multi_hop_filter() {
    let (sql, params) = sql(r#"users[manager.manager.name == "X"] { name }"#);
    assert_sql_eq(
        &sql,
        r#"SELECT "$$users3"."name" FROM "users" AS "$$users3" WHERE ((SELECT (SELECT "$$users1"."name" FROM "users" AS "$$users1" WHERE ("$$users1"."id" = "$$users0"."manager_id")) AS "$$val2" FROM "users" AS "$$users0" WHERE ("$$users0"."id" = "$$users3"."manager_id")) = ?)"#,
    );
    assert_eq!(params, vec![str_param("X")]);
}

#[test]
fn nav_one_to_one_back_ref_filter() {
    let (sql, params) = sql(r#"users[settings.theme == "dark"] { name }"#);
    assert_sql_eq(
        &sql,
        r#"SELECT "$$users1"."name" FROM "users" AS "$$users1" WHERE ((SELECT "$$settings0"."theme" FROM "settings" AS "$$settings0" WHERE ("$$settings0"."user_id" = "$$users1"."id")) = ?)"#,
    );
    assert_eq!(params, vec![str_param("dark")]);
}

#[test]
fn nav_sort_key() {
    let (sql, params) = sql("users.sort(manager.name) { name }");
    assert_sql_eq(
        &sql,
        r#"SELECT "$$users1"."name" FROM "users" AS "$$users1" ORDER BY (SELECT "$$users0"."name" FROM "users" AS "$$users0" WHERE ("$$users0"."id" = "$$users1"."manager_id")) ASC"#,
    );
    assert!(params.is_empty());
}

#[test]
fn flattened_root_scalar() {
    let (sql, params) = sql("users[active].name");
    assert_sql_eq(
        &sql,
        r#"SELECT "$$users0"."name" FROM "users" AS "$$users0" WHERE "$$users0"."active""#,
    );
    assert!(params.is_empty());
}

#[test]
fn flatten_collection_bare() {
    let (sql, params) = sql("users.orders.flatten()");
    assert_sql_eq(
        &sql,
        r#"SELECT "$$orders1".* FROM "orders" AS "$$orders1" WHERE ("$$orders1"."user_id" IN (SELECT "$$users0"."id" FROM "users" AS "$$users0"))"#,
    );
    assert!(params.is_empty());
}

#[test]
fn flatten_collection_first() {
    let (sql, params) = sql("users.orders.flatten().first()");
    assert_sql_eq(
        &sql,
        r#"SELECT "$$orders1".* FROM "orders" AS "$$orders1" WHERE ("$$orders1"."user_id" IN (SELECT "$$users0"."id" FROM "users" AS "$$users0")) LIMIT ?"#,
    );
    assert_eq!(params, vec![int_param(1)]);
}

#[test]
fn flatten_collection_pipeline() {
    let (take_sql, take_params) = case_pos_guard(2, "take");
    let (sql, params) = sql("users[active].orders.flatten().sort_desc(total).take(2)");
    assert_sql_eq(
        &sql,
        &format!(
            r#"SELECT "$$orders1".* FROM "orders" AS "$$orders1" WHERE ("$$orders1"."user_id" IN (SELECT "$$users0"."id" FROM "users" AS "$$users0" WHERE "$$users0"."active")) ORDER BY "$$orders1"."total" DESC LIMIT ({take_sql})"#
        ),
    );
    assert_eq!(params, take_params);
}

#[test]
fn flatten_single_ref_collapse() {
    let (sql, params) = sql("users.manager.first()");
    assert_sql_eq(
        &sql,
        r#"SELECT "$$users1".* FROM "users" AS "$$users1" WHERE ("$$users1"."id" IN (SELECT "$$users0"."manager_id" FROM "users" AS "$$users0")) LIMIT ?"#,
    );
    assert_eq!(params, vec![int_param(1)]);
}

#[test]
fn flatten_single_ref_bare() {
    let (sql, params) = sql("users.manager");
    assert_sql_eq(
        &sql,
        r#"SELECT "$$users1".* FROM "users" AS "$$users1" WHERE ("$$users1"."id" IN (SELECT "$$users0"."manager_id" FROM "users" AS "$$users0"))"#,
    );
    assert!(params.is_empty());
}

#[test]
fn flattened_scalar_over_navigation() {
    let (sql, params) = sql("users.manager.name");
    assert_sql_eq(
        &sql,
        r#"SELECT "$$users1"."name" FROM "users" AS "$$users1" WHERE ("$$users1"."id" IN (SELECT "$$users0"."manager_id" FROM "users" AS "$$users0"))"#,
    );
    assert!(params.is_empty());
}

#[test]
fn flatten_multi_hop_navigation() {
    let (sql, params) = sql("users.manager.orders.flatten()");
    assert_sql_eq(
        &sql,
        r#"SELECT "$$orders2".* FROM "orders" AS "$$orders2" WHERE ("$$orders2"."user_id" IN (SELECT "$$users1"."id" FROM "users" AS "$$users1" WHERE ("$$users1"."id" IN (SELECT "$$users0"."manager_id" FROM "users" AS "$$users0"))))"#,
    );
    assert!(params.is_empty());
}

#[test]
fn flatten_mixed_direction_chain() {
    let (sql, params) = sql("users.settings.user.orders.flatten()");
    assert_sql_eq(
        &sql,
        r#"SELECT "$$orders3".* FROM "orders" AS "$$orders3" WHERE ("$$orders3"."user_id" IN (SELECT "$$users2"."id" FROM "users" AS "$$users2" WHERE ("$$users2"."id" IN (SELECT "$$settings1"."user_id" FROM "settings" AS "$$settings1" WHERE ("$$settings1"."user_id" IN (SELECT "$$users0"."id" FROM "users" AS "$$users0"))))))"#,
    );
    assert!(params.is_empty());
}

#[test]
fn lit_nested_flatten() {
    let compiled = compile("[[1, 3], [2, 4]].flatten()");
    assert_eq!(compiled.statements.len(), 1);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"SELECT "lit1"."c1" AS "c0" FROM "$$lit_2_II"('lit1') AS "lit1" WHERE ("lit1"."c0" IN (SELECT "lit0"."c0" FROM "$$lit_1_I"('lit0') AS "lit0"))"#,
    );
    assert!(compiled.statements[0].params.is_empty());
    let mut expected = HashMap::new();
    expected.insert(
        "lit0".to_string(),
        vec![vec![ParamValue::Int(0)], vec![ParamValue::Int(1)]],
    );
    expected.insert(
        "lit1".to_string(),
        vec![
            vec![ParamValue::Int(0), ParamValue::Int(1)],
            vec![ParamValue::Int(0), ParamValue::Int(3)],
            vec![ParamValue::Int(1), ParamValue::Int(2)],
            vec![ParamValue::Int(1), ParamValue::Int(4)],
        ],
    );
    assert_eq!(compiled.literals, expected);
}

#[test]
fn lit_computed_shapes() {
    let (sql1, params1) = sql("[1 + 2]");
    assert_sql_eq(
        &sql1,
        r#"SELECT "lit0".* FROM (SELECT "$$add"(?, ?) AS "c0") AS "lit0""#,
    );
    assert_eq!(params1, vec![int_param(1), int_param(2)]);

    let (sql2, params2) = sql("[(1 + 2, 3)]");
    assert_sql_eq(
        &sql2,
        r#"SELECT "lit0".* FROM (SELECT "$$add"(?, ?) AS "c0", ? AS "c1") AS "lit0""#,
    );
    assert_eq!(params2, vec![int_param(1), int_param(2), int_param(3)]);

    let (sql3, params3) = sql("[{a = 1 + 2}]");
    assert_sql_eq(
        &sql3,
        r#"SELECT "lit0".* FROM (SELECT "$$add"(?, ?) AS "c0") AS "lit0""#,
    );
    assert_eq!(params3, vec![int_param(1), int_param(2)]);

    let (sql4, params4) = sql("[if true {1} else {2}]");
    assert_sql_eq(
        &sql4,
        r#"SELECT "lit0".* FROM (SELECT (CASE WHEN ? THEN ? ELSE ? END) AS "c0") AS "lit0""#,
    );
    assert_eq!(
        params4,
        vec![
            Param::Value(ParamValue::Bool(true)),
            int_param(1),
            int_param(2)
        ]
    );
}

#[test]
fn lit_flat_int_result() {
    let compiled = compile("[1, 2, 3]");
    assert_eq!(compiled.statements.len(), 1);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"SELECT "lit0".* FROM "$$lit_1_I"('lit0') AS "lit0""#,
    );
    assert!(compiled.statements[0].params.is_empty());
    let mut expected = HashMap::new();
    expected.insert(
        "lit0".to_string(),
        vec![
            vec![ParamValue::Int(1)],
            vec![ParamValue::Int(2)],
            vec![ParamValue::Int(3)],
        ],
    );
    assert_eq!(compiled.literals, expected);
}

#[test]
fn lit_flat_string_result() {
    let compiled = compile(r#"["b", "a"]"#);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"SELECT "lit0".* FROM "$$lit_1_T"('lit0') AS "lit0""#,
    );
    let mut expected = HashMap::new();
    expected.insert(
        "lit0".to_string(),
        vec![
            vec![ParamValue::String("b".to_string())],
            vec![ParamValue::String("a".to_string())],
        ],
    );
    assert_eq!(compiled.literals, expected);
}

#[test]
fn lit_negated_elements() {
    let (sql, params) = sql("[-7, 3]");
    assert_sql_eq(
        &sql,
        r#"SELECT "lit0".* FROM (SELECT "$$neg"(?) AS "c0" UNION ALL SELECT ? AS "c0") AS "lit0""#,
    );
    assert_eq!(params, vec![int_param(7), int_param(3)]);
}

#[test]
fn lit_optional_elements() {
    let compiled = compile("[some(1), none]");
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"SELECT "lit0".* FROM "$$lit_1_I"('lit0') AS "lit0""#,
    );
    let mut expected = HashMap::new();
    expected.insert(
        "lit0".to_string(),
        vec![vec![ParamValue::Int(1)], vec![ParamValue::Null]],
    );
    assert_eq!(compiled.literals, expected);
}

#[test]
fn lit_tuple_elements() {
    let compiled = compile(r#"[(1, "a"), (2, "b")]"#);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"SELECT "lit0".* FROM "$$lit_2_IT"('lit0') AS "lit0""#,
    );
    let mut expected = HashMap::new();
    expected.insert(
        "lit0".to_string(),
        vec![
            vec![ParamValue::Int(1), ParamValue::String("a".to_string())],
            vec![ParamValue::Int(2), ParamValue::String("b".to_string())],
        ],
    );
    assert_eq!(compiled.literals, expected);
}

#[test]
fn lit_nested_result() {
    let compiled = compile("[[1, 3], [2, 4]]");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"SELECT "lit0"."c0" FROM "$$lit_1_I"('lit0') AS "lit0""#,
    );
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "lit1".* FROM "$$lit_2_II"('lit1') AS "lit1" WHERE ("lit1"."c0" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"()))"#,
    );
    assert_eq!(
        compiled.statements[0].shape.columns,
        vec![super::model::OutputColumn {
            name: "c0".to_string(),
            role: super::model::ColumnRole::Key,
            ty: None,
        }]
    );
    let mut expected = HashMap::new();
    expected.insert(
        "lit0".to_string(),
        vec![vec![ParamValue::Int(0)], vec![ParamValue::Int(1)]],
    );
    expected.insert(
        "lit1".to_string(),
        vec![
            vec![ParamValue::Int(0), ParamValue::Int(1)],
            vec![ParamValue::Int(0), ParamValue::Int(3)],
            vec![ParamValue::Int(1), ParamValue::Int(2)],
            vec![ParamValue::Int(1), ParamValue::Int(4)],
        ],
    );
    assert_eq!(compiled.literals, expected);
}

#[test]
fn lit_ragged_result() {
    let compiled = compile("[[1], [2, 3]]");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"SELECT "lit0"."c0" FROM "$$lit_1_I"('lit0') AS "lit0""#,
    );
    let mut expected = HashMap::new();
    expected.insert(
        "lit0".to_string(),
        vec![vec![ParamValue::Int(0)], vec![ParamValue::Int(1)]],
    );
    expected.insert(
        "lit1".to_string(),
        vec![
            vec![ParamValue::Int(0), ParamValue::Int(1)],
            vec![ParamValue::Int(1), ParamValue::Int(2)],
            vec![ParamValue::Int(1), ParamValue::Int(3)],
        ],
    );
    assert_eq!(compiled.literals, expected);
}

#[test]
fn lit_empty_inner_result() {
    let compiled = compile("[[], [1]]");
    assert_eq!(compiled.statements.len(), 2);
    let mut expected = HashMap::new();
    expected.insert(
        "lit0".to_string(),
        vec![vec![ParamValue::Int(0)], vec![ParamValue::Int(1)]],
    );
    expected.insert(
        "lit1".to_string(),
        vec![vec![ParamValue::Int(1), ParamValue::Int(1)]],
    );
    assert_eq!(compiled.literals, expected);
}

#[test]
fn lit_contains() {
    let (sql, params) = sql("[1, 2, 3].contains(2)");
    assert_sql_eq(
        &sql,
        r#"SELECT EXISTS (SELECT "lit1".* FROM "$$lit_1_I"('lit1') AS "lit1" WHERE ("lit1"."c0" = ?)) AS "$$val0""#,
    );
    assert_eq!(params, vec![int_param(2)]);
}

#[test]
fn lit_sum() {
    let (sql, params) = sql("[1, 2, 3].sum()");
    assert_sql_eq(
        &sql,
        r#"SELECT "SUM"("lit1"."c0") AS "$$val0" FROM "$$lit_1_I"('lit1') AS "lit1""#,
    );
    assert!(params.is_empty());
}

#[test]
fn lit_dec_sum() {
    let (sql, params) = sql("[1.5, 2.5].sum()");
    assert_sql_eq(
        &sql,
        r#"SELECT "$$sum_dec"("lit1"."c0") AS "$$val0" FROM "$$lit_1_B"('lit1') AS "lit1""#,
    );
    assert!(params.is_empty());
}

#[test]
fn lit_len() {
    let (sql, params) = sql("[1, 2, 3].len()");
    assert_sql_eq(
        &sql,
        r#"SELECT COUNT(*) AS "$$val0" FROM "$$lit_1_I"('lit1') AS "lit1""#,
    );
    assert!(params.is_empty());
}

#[test]
fn lit_nested_len() {
    let (sql, params) = sql("[[1, 3], [2, 4]].len()");
    assert_sql_eq(
        &sql,
        r#"SELECT COUNT(*) AS "$$val0" FROM "$$lit_1_I"('lit1') AS "lit1""#,
    );
    assert!(params.is_empty());
}

#[test]
fn lit_first() {
    let (sql, params) = sql("[1, 2, 3].first()");
    assert_sql_eq(
        &sql,
        r#"SELECT (SELECT "lit1".* FROM "$$lit_1_I"('lit1') AS "lit1" LIMIT ?) AS "$$val0""#,
    );
    assert_eq!(params, vec![int_param(1)]);
}

#[test]
fn lit_take() {
    let (guard_sql, guard_params) = case_pos_guard(2, "take");
    let (sql, params) = sql("[1, 2, 3].take(2)");
    assert_sql_eq(
        &sql,
        &format!(r#"SELECT "lit0".* FROM "$$lit_1_I"('lit0') AS "lit0" LIMIT ({guard_sql})"#),
    );
    assert_eq!(params, guard_params);
}

#[test]
fn lit_tuple_result() {
    let (sql, params) = sql(r#"(1, "hello")"#);
    assert_sql_eq(&sql, r#"SELECT ? AS "c0", ? AS "c1""#);
    assert_eq!(params, vec![int_param(1), str_param("hello")]);
}

#[test]
fn lit_broadcast_item() {
    let compiled = compile("users { x = [1, 2, 3] }");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"SELECT "$$users0".* FROM "users" AS "$$users0""#,
    );
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "lit1".* FROM "$$lit_1_I"('lit1') AS "lit1""#,
    );
    let mut expected = HashMap::new();
    expected.insert(
        "lit1".to_string(),
        vec![
            vec![ParamValue::Int(1)],
            vec![ParamValue::Int(2)],
            vec![ParamValue::Int(3)],
        ],
    );
    assert_eq!(compiled.literals, expected);
}

#[test]
fn lit_scalar_item() {
    let (sql, params) = sql("users { s = [1, 2, 3].sum() }");
    assert_sql_eq(
        &sql,
        r#"SELECT (SELECT "SUM"("lit1"."c0") AS "$$val2" FROM "$$lit_1_I"('lit1') AS "lit1") AS "s" FROM "users" AS "$$users0""#,
    );
    assert!(params.is_empty());
}

#[test]
fn lit_correlated_temp() {
    use crate::codegen::model::{Cardinality, ChildRef, ParentRef, ResultShape};
    let compiled = compile("users { x = [age, 1] }");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"CREATE TEMP TABLE "$$split0" AS SELECT "$$users0".*, "ROW_NUMBER"() OVER () AS "$$key" FROM "users" AS "$$users0""#,
    );
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$split0"."$$key" FROM "$$split0" AS "$$split0""#,
    );
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$split0"."$$key" AS "c0", "$$split0"."age" AS "c1" FROM "$$split0" AS "$$split0" UNION ALL SELECT "$$split0"."$$key" AS "c0", ? AS "c1" FROM "$$split0" AS "$$split0""#,
    );
    assert_eq!(compiled.statements[2].params, vec![int_param(1)]);
    assert_eq!(
        compiled.result,
        ResultShape::List {
            element: Box::new(ResultShape::Record(vec![(
                "x".to_string(),
                ResultShape::List {
                    element: Box::new(ResultShape::Column("c1".to_string())),
                    child: ChildRef {
                        stmt: 2,
                        parent: Some(ParentRef {
                            stmt: 0,
                            parent_key: "$$key".to_string(),
                            child_key: "c0".to_string(),
                            negated: false,
                        }),
                        cardinality: Cardinality::Many,
                        tag: None,
                    },
                }
            ),])),
            child: ChildRef {
                stmt: 1,
                parent: None,
                cardinality: Cardinality::Many,
                tag: None,
            },
        }
    );
}

#[test]
fn lit_nested_correlated() {
    use crate::codegen::model::{Cardinality, ChildRef, ParentRef, ResultShape};
    let compiled = compile("users { x = [[age]] }");
    assert_eq!(compiled.statements.len(), 4);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"CREATE TEMP TABLE "$$split0" AS SELECT "$$users0".*, "ROW_NUMBER"() OVER () AS "$$key" FROM "users" AS "$$users0""#,
    );
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"CREATE TEMP TABLE "$$split0" AS SELECT "$$subq0".*, "ROW_NUMBER"() OVER () AS "$$key" FROM (SELECT "$$split1"."$$key" AS "c0", ? AS "c1", "$$split1"."age" FROM "$$split1" AS "$$split1") AS "$$subq0""#,
    );
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$split0"."$$key" FROM "$$split0" AS "$$split0""#,
    );
    assert_sql_eq(
        &compiled.statements[3].sql,
        r#"SELECT "$$split0"."$$key" AS "c0", "$$split0"."age" AS "c1" FROM "$$split0" AS "$$split0" WHERE ("$$split0"."c1" = ?)"#,
    );
    assert_eq!(compiled.statements[3].params, vec![int_param(0)]);
    assert_eq!(
        compiled.result,
        ResultShape::List {
            element: Box::new(ResultShape::Record(vec![(
                "x".to_string(),
                ResultShape::List {
                    element: Box::new(ResultShape::List {
                        element: Box::new(ResultShape::Column("c1".to_string())),
                        child: ChildRef {
                            stmt: 3,
                            parent: Some(ParentRef {
                                stmt: 1,
                                parent_key: "$$key".to_string(),
                                child_key: "c0".to_string(),
                                negated: false,
                            }),
                            cardinality: Cardinality::Many,
                            tag: None,
                        },
                    }),
                    child: ChildRef {
                        stmt: 1,
                        parent: Some(ParentRef {
                            stmt: 0,
                            parent_key: "$$key".to_string(),
                            child_key: "c0".to_string(),
                            negated: false,
                        }),
                        cardinality: Cardinality::Many,
                        tag: None,
                    },
                }
            ),])),
            child: ChildRef {
                stmt: 2,
                parent: None,
                cardinality: Cardinality::Many,
                tag: None,
            },
        }
    );
}

#[test]
fn lit_correlated_take() {
    use crate::codegen::model::{Cardinality, ChildRef, ParentRef, ResultShape};
    let compiled = compile("users { x = [age, age + 1].take(1) }");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"CREATE TEMP TABLE "$$split0" AS SELECT "$$users0".*, "ROW_NUMBER"() OVER () AS "$$key" FROM "users" AS "$$users0""#,
    );
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$split0"."$$key" FROM "$$split0" AS "$$split0""#,
    );
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$subq0".* FROM (SELECT "$$subq1".*, "ROW_NUMBER"() OVER (PARTITION BY "$$subq1"."c0") AS "$$rn" FROM (SELECT "$$split0"."$$key" AS "c0", "$$split0"."age" AS "c1" FROM "$$split0" AS "$$split0" UNION ALL SELECT "$$split0"."$$key" AS "c0", "$$add"("$$split0"."age", ?) AS "c1" FROM "$$split0" AS "$$split0") AS "$$subq1") AS "$$subq0" WHERE ("$$subq0"."$$rn" <= (CASE WHEN ? >= ? THEN ? ELSE "$$panic"(?) END))"#,
    );
    assert_eq!(
        compiled.result,
        ResultShape::List {
            element: Box::new(ResultShape::Record(vec![(
                "x".to_string(),
                ResultShape::List {
                    element: Box::new(ResultShape::Column("c1".to_string())),
                    child: ChildRef {
                        stmt: 2,
                        parent: Some(ParentRef {
                            stmt: 0,
                            parent_key: "$$key".to_string(),
                            child_key: "c0".to_string(),
                            negated: false,
                        }),
                        cardinality: Cardinality::Many,
                        tag: None,
                    },
                }
            ),])),
            child: ChildRef {
                stmt: 1,
                parent: None,
                cardinality: Cardinality::Many,
                tag: None,
            },
        }
    );
}

#[test]
fn lit_correlated_scope_needle() {
    let compiled = compile("users { x = [age, age + 1].contains(age) }");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT EXISTS (SELECT "$$subq0".* FROM (SELECT "$$split0"."$$key" AS "c0", "$$split0"."age" AS "c1" FROM "$$split0" AS "$$split0" UNION ALL SELECT "$$split0"."$$key" AS "c0", "$$add"("$$split0"."age", ?) AS "c1" FROM "$$split0" AS "$$split0") AS "$$subq0" WHERE ("$$subq0"."c0" = "$$split0"."$$key") AND ("$$subq0"."c1" = "$$split0"."age")) AS "x", "$$split0"."$$key" FROM "$$split0" AS "$$split0""#,
    );
}

#[test]
fn lit_correlated_sum() {
    use crate::codegen::model::{Cardinality, ChildRef, ResultShape};
    let compiled = compile("users { x = [age, age + 1].sum() }");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"CREATE TEMP TABLE "$$split0" AS SELECT "$$users0".*, "ROW_NUMBER"() OVER () AS "$$key" FROM "users" AS "$$users0""#,
    );
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT (SELECT "SUM"("$$subq0"."c1") AS "$$val0" FROM (SELECT "$$split0"."$$key" AS "c0", "$$split0"."age" AS "c1" FROM "$$split0" AS "$$split0" UNION ALL SELECT "$$split0"."$$key" AS "c0", "$$add"("$$split0"."age", ?) AS "c1" FROM "$$split0" AS "$$split0") AS "$$subq0" WHERE ("$$subq0"."c0" = "$$split0"."$$key")) AS "x", "$$split0"."$$key" FROM "$$split0" AS "$$split0""#,
    );
    assert_eq!(
        compiled.result,
        ResultShape::List {
            element: Box::new(ResultShape::Record(vec![(
                "x".to_string(),
                ResultShape::Column("x".to_string()),
            ),])),
            child: ChildRef {
                stmt: 1,
                parent: None,
                cardinality: Cardinality::Many,
                tag: None,
            },
        }
    );
}

#[test]
fn lit_correlated_flatten() {
    use crate::codegen::model::{Cardinality, ChildRef, ParentRef, ResultShape};
    let compiled = compile("users { x = [[age], [age + 1]].flatten() }");
    assert_eq!(compiled.statements.len(), 4);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"CREATE TEMP TABLE "$$split0" AS SELECT "$$users0".*, "ROW_NUMBER"() OVER () AS "$$key" FROM "users" AS "$$users0""#,
    );
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"CREATE TEMP TABLE "$$split0" AS SELECT "$$subq0".*, "ROW_NUMBER"() OVER () AS "$$key" FROM (SELECT "$$split1"."$$key" AS "c0", ? AS "c1", "$$split1"."age" FROM "$$split1" AS "$$split1" UNION ALL SELECT "$$split1"."$$key" AS "c0", ? AS "c1", "$$split1"."age" FROM "$$split1" AS "$$split1") AS "$$subq0""#,
    );
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$split0"."$$key" FROM "$$split0" AS "$$split0""#,
    );
    assert_sql_eq(
        &compiled.statements[3].sql,
        r#"SELECT "$$split0"."c0", "$$split0"."age" AS "c1" FROM "$$split0" AS "$$split0" WHERE ("$$split0"."c1" = ?) UNION ALL SELECT "$$split0"."c0", "$$add"("$$split0"."age", ?) AS "c1" FROM "$$split0" AS "$$split0" WHERE ("$$split0"."c1" = ?)"#,
    );
    assert_eq!(
        compiled.result,
        ResultShape::List {
            element: Box::new(ResultShape::Record(vec![(
                "x".to_string(),
                ResultShape::List {
                    element: Box::new(ResultShape::Column("c1".to_string())),
                    child: ChildRef {
                        stmt: 3,
                        parent: Some(ParentRef {
                            stmt: 0,
                            parent_key: "$$key".to_string(),
                            child_key: "c0".to_string(),
                            negated: false,
                        }),
                        cardinality: Cardinality::Many,
                        tag: None,
                    },
                }
            ),])),
            child: ChildRef {
                stmt: 2,
                parent: None,
                cardinality: Cardinality::Many,
                tag: None,
            },
        }
    );
}

#[test]
fn lit_scalar_temp_single() {
    let (sql_first, _) = sql("users { x = [{a = age}].first()?.a }");
    assert_sql_eq(
        &sql_first,
        r#"SELECT (SELECT "$$users0"."age" AS "$$val1") AS "x" FROM "users" AS "$$users0""#,
    );
    let (sql_nth, _) = sql("users { x = [{a = age }, {a = age + 1}].nth(1)?.a }");
    assert_sql_eq(
        &sql_nth,
        r#"SELECT (SELECT "$$add"("$$users0"."age", ?) AS "$$val1") AS "x" FROM "users" AS "$$users0""#,
    );
    let (sql_lit_nth, _) = sql("users { x = [{a = 1}, {a = 2}].nth(1)?.a }");
    assert_sql_eq(
        &sql_lit_nth,
        r#"SELECT (SELECT "lit1"."c0" FROM "$$lit_1_I"('lit1') AS "lit1" LIMIT ? OFFSET (CASE WHEN ? >= ? THEN ? ELSE "$$panic"(?) END)) AS "x" FROM "users" AS "$$users0""#,
    );
    let compiled = compile("users { x = [age, age + 1].nth(age - 30) }");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT (SELECT "$$subq4"."c1" AS "$$val3" FROM (SELECT "$$subq2"."c0", "$$subq2"."c1", "ROW_NUMBER"() OVER () AS "$$rn" FROM (SELECT "$$split1"."$$key" AS "c0", "$$split1"."age" AS "c1" FROM "$$split1" AS "$$split1" UNION ALL SELECT "$$split1"."$$key" AS "c0", "$$add"("$$split1"."age", ?) AS "c1" FROM "$$split1" AS "$$split1") AS "$$subq2" WHERE ("$$subq2"."c0" = "$$split1"."$$key")) AS "$$subq4" WHERE ("$$subq4"."$$rn" = "$$add"(CASE WHEN "$$sub"((SELECT "$$split1"."age" FROM "$$split1" AS "$$split1" WHERE ("$$split1"."$$key" = "$$subq4"."c0")), ?) >= ? THEN "$$sub"((SELECT "$$split1"."age" FROM "$$split1" AS "$$split1" WHERE ("$$split1"."$$key" = "$$subq4"."c0")), ?) ELSE "$$panic"(?) END, ?))) AS "x", "$$split1"."$$key" FROM "$$split1" AS "$$split1""#,
    );
}

#[test]
fn lit_take_sort_order() {
    let compiled = compile("users { x = [{a = 3}, {a = 1}, {a = 2}].take(2).sort_asc(a) }");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$subq4".* FROM (SELECT "$$subq3".* FROM (SELECT "$$subq2".*, "ROW_NUMBER"() OVER (PARTITION BY "$$subq2"."c0") AS "$$rn" FROM (SELECT "$$split1"."$$key" AS "c0", ? AS "c1" FROM "$$split1" AS "$$split1" UNION ALL SELECT "$$split1"."$$key" AS "c0", ? AS "c1" FROM "$$split1" AS "$$split1" UNION ALL SELECT "$$split1"."$$key" AS "c0", ? AS "c1" FROM "$$split1" AS "$$split1") AS "$$subq2") AS "$$subq3" WHERE ("$$subq3"."$$rn" <= (CASE WHEN ? >= ? THEN ? ELSE "$$panic"(?) END))) AS "$$subq4" ORDER BY "$$subq4"."c1" ASC"#,
    );
}

#[test]
fn lit_computed_tuple_if() {
    let compiled = compile("users { x = [if true {(1, 2)} else {(3, 4)}] }");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "lit1".* FROM (SELECT (CASE WHEN ? THEN ? ELSE ? END) AS "c0", (CASE WHEN ? THEN ? ELSE ? END) AS "c1") AS "lit1""#,
    );
}

#[test]
fn lit_global_query_elements() {
    let compiled = compile("users { x = [(users.first()?.name)] }");
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$split1"."$$key" AS "c0", (SELECT "$$users2"."name" FROM "users" AS "$$users2" LIMIT ?) AS "c1" FROM "$$split1" AS "$$split1""#,
    );
}

#[test]
fn lit_correlated_element_filter() {
    let compiled = compile("users { x = [{a = age}, {a = age + 1}].filter(a > 30) }");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$subq0".* FROM (SELECT "$$split0"."$$key" AS "c0", "$$split0"."age" AS "c1" FROM "$$split0" AS "$$split0" UNION ALL SELECT "$$split0"."$$key" AS "c0", "$$add"("$$split0"."age", ?) AS "c1" FROM "$$split0" AS "$$split0") AS "$$subq0" WHERE ("$$subq0"."c1" > ?)"#,
    );
    assert_eq!(
        compiled.statements[2].params,
        vec![int_param(1), int_param(30)]
    );
}

#[test]
fn lit_correlated_scalar_method() {
    let compiled = compile("users { x = [name.len()] }");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_contains(&compiled.statements[2].sql, r#""length"("#);
    assert!(compiled.statements[2].params.is_empty());
}

#[test]
fn lit_correlated_navigation() {
    let compiled = compile("users { x = [manager.name] }");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_contains(&compiled.statements[2].sql, "manager_id");
    assert_sql_contains(&compiled.statements[2].sql, r#"FROM "users""#);
}

#[test]
fn lit_dynamic_computed() {
    let (sql, params) = sql("[@now]");
    assert_sql_eq(
        &sql,
        r#"SELECT "lit0".* FROM (SELECT "$$now"() AS "c0") AS "lit0""#,
    );
    assert!(params.is_empty());
}

#[test]
fn lit_neg_overflow_computed() {
    let (sql, params) = sql("[-Int::MIN]");
    assert_sql_eq(
        &sql,
        r#"SELECT "lit0".* FROM (SELECT "$$neg"(?) AS "c0") AS "lit0""#,
    );
    assert_eq!(params, vec![int_param(i64::MIN)]);
}

#[test]
fn some_presence_transparent() {
    let (sql, params) = sql("some([1, 2])");
    assert_sql_eq(
        &sql,
        r#"SELECT "lit0".* FROM "$$lit_1_I"('lit0') AS "lit0""#,
    );
    assert!(params.is_empty());
}

#[test]
fn some_correlated_temp_item() {
    let compiled = compile("users { x = some([age]) }");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"CREATE TEMP TABLE "$$split1" AS SELECT "$$users0".*, "ROW_NUMBER"() OVER () AS "$$key" FROM "users" AS "$$users0""#,
    );
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$split1"."$$key" AS "c0", "$$split1"."age" AS "c1" FROM "$$split1" AS "$$split1""#,
    );
}

#[test]
fn if_broadcast_item() {
    let compiled = compile("users { x = if true { [1] } else { [2] } }");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"SELECT "$$users0".* FROM "users" AS "$$users0""#,
    );
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "lit1".* FROM "$$lit_1_I"('lit1') AS "lit1""#,
    );
}

#[test]
fn via_root_nesting() {
    use crate::codegen::model::{Cardinality, ChildRef, ParentRef, ResultShape};
    let compiled = compile("users.scores");
    assert_eq!(compiled.statements.len(), 2);
    let inner = ChildRef {
        stmt: 1,
        parent: Some(ParentRef {
            stmt: 0,
            parent_key: "id".to_string(),
            child_key: "user_key".to_string(),
            negated: false,
        }),
        cardinality: Cardinality::Many,
        tag: None,
    };
    assert_eq!(
        compiled.result,
        ResultShape::List {
            element: Box::new(ResultShape::List {
                element: Box::new(ResultShape::Column("score_value".to_string())),
                child: inner.clone(),
            }),
            child: inner,
        }
    );
}

#[test]
fn via_root_take_outer() {
    let (guard_sql, guard_params) = case_pos_guard(2, "take");
    let compiled = compile("users.scores.take(2)");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[0].sql,
        &format!(r#"SELECT "$$users0"."id" FROM "users" AS "$$users0" LIMIT ({guard_sql})"#),
    );
    assert_eq!(compiled.statements[0].params, guard_params);
}

#[test]
fn lit_single_scalar_element() {
    use crate::codegen::model::{Cardinality, ChildRef, ResultShape};
    let compiled = compile("[1, 2, 3]");
    assert_eq!(compiled.statements.len(), 1);
    assert_eq!(
        compiled.result,
        ResultShape::List {
            element: Box::new(ResultShape::Column("c0".to_string())),
            child: ChildRef {
                stmt: 0,
                parent: None,
                cardinality: Cardinality::Many,
                tag: None,
            },
        }
    );
}

#[test]
fn lit_nested_scalar_elements() {
    use crate::codegen::model::{Cardinality, ChildRef, ParentRef, ResultShape};
    let compiled = compile("[[1, 3], [2, 4]]");
    assert_eq!(compiled.statements.len(), 2);
    let child = ChildRef {
        stmt: 1,
        parent: Some(ParentRef {
            stmt: 0,
            parent_key: "c0".to_string(),
            child_key: "c0".to_string(),
            negated: false,
        }),
        cardinality: Cardinality::Many,
        tag: None,
    };
    assert_eq!(
        compiled.result,
        ResultShape::List {
            element: Box::new(ResultShape::List {
                element: Box::new(ResultShape::Column("c1".to_string())),
                child: child.clone(),
            }),
            child,
        }
    );
}

#[test]
fn lit_deep_scalar_leaf() {
    use crate::codegen::model::{Cardinality, ChildRef, ParentRef, ResultShape};
    let compiled = compile("[[[1]]]");
    assert_eq!(compiled.statements.len(), 3);
    assert_eq!(
        compiled.result,
        ResultShape::List {
            element: Box::new(ResultShape::List {
                element: Box::new(ResultShape::List {
                    element: Box::new(ResultShape::Column("c1".to_string())),
                    child: ChildRef {
                        stmt: 2,
                        parent: Some(ParentRef {
                            stmt: 1,
                            parent_key: "c1".to_string(),
                            child_key: "c0".to_string(),
                            negated: false,
                        }),
                        cardinality: Cardinality::Many,
                        tag: None,
                    },
                }),
                child: ChildRef {
                    stmt: 1,
                    parent: Some(ParentRef {
                        stmt: 0,
                        parent_key: "c0".to_string(),
                        child_key: "c0".to_string(),
                        negated: false,
                    }),
                    cardinality: Cardinality::Many,
                    tag: None,
                },
            }),
            child: ChildRef {
                stmt: 1,
                parent: Some(ParentRef {
                    stmt: 0,
                    parent_key: "c0".to_string(),
                    child_key: "c0".to_string(),
                    negated: false,
                }),
                cardinality: Cardinality::Many,
                tag: None,
            },
        }
    );
}

#[test]
fn lit_tuple_middle_nesting() {
    use crate::codegen::model::{Cardinality, ChildRef, ParentRef, ResultShape};
    let compiled = compile("[[(20, [1, 2])], [(30, [3, 4, 5])]]");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "lit1".* FROM "$$lit_3_III"('lit1') AS "lit1" WHERE ("lit1"."c0" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"()))"#,
    );
    let mut expected = HashMap::new();
    expected.insert(
        "lit0".to_string(),
        vec![vec![ParamValue::Int(0)], vec![ParamValue::Int(1)]],
    );
    expected.insert(
        "lit1".to_string(),
        vec![
            vec![ParamValue::Int(0), ParamValue::Int(0), ParamValue::Int(20)],
            vec![ParamValue::Int(1), ParamValue::Int(1), ParamValue::Int(30)],
        ],
    );
    expected.insert(
        "lit2".to_string(),
        vec![
            vec![ParamValue::Int(0), ParamValue::Int(1)],
            vec![ParamValue::Int(0), ParamValue::Int(2)],
            vec![ParamValue::Int(1), ParamValue::Int(3)],
            vec![ParamValue::Int(1), ParamValue::Int(4)],
            vec![ParamValue::Int(1), ParamValue::Int(5)],
        ],
    );
    assert_eq!(compiled.literals, expected);
    assert_eq!(
        compiled.result,
        ResultShape::List {
            element: Box::new(ResultShape::List {
                element: Box::new(ResultShape::Tuple(vec![
                    ResultShape::Column("c2".to_string()),
                    ResultShape::List {
                        element: Box::new(ResultShape::Column("c1".to_string())),
                        child: ChildRef {
                            stmt: 2,
                            parent: Some(ParentRef {
                                stmt: 1,
                                parent_key: "c1".to_string(),
                                child_key: "c0".to_string(),
                                negated: false,
                            }),
                            cardinality: Cardinality::Many,
                            tag: None,
                        },
                    },
                ])),
                child: ChildRef {
                    stmt: 1,
                    parent: Some(ParentRef {
                        stmt: 0,
                        parent_key: "c0".to_string(),
                        child_key: "c0".to_string(),
                        negated: false,
                    }),
                    cardinality: Cardinality::Many,
                    tag: None,
                },
            }),
            child: ChildRef {
                stmt: 1,
                parent: Some(ParentRef {
                    stmt: 0,
                    parent_key: "c0".to_string(),
                    child_key: "c0".to_string(),
                    negated: false,
                }),
                cardinality: Cardinality::Many,
                tag: None,
            },
        }
    );
}

#[test]
fn lit_multi_list_tuple() {
    use crate::codegen::model::{Cardinality, ChildRef, ResultShape};
    let compiled = compile("[([1], [2])]");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(&compiled.statements[0].sql, r#"SELECT ? AS "c0""#);
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "lit1".* FROM "$$lit_1_I"('lit1') AS "lit1""#,
    );
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "lit0".* FROM "$$lit_1_I"('lit0') AS "lit0""#,
    );
    assert_eq!(
        compiled.result,
        ResultShape::List {
            element: Box::new(ResultShape::Tuple(vec![
                ResultShape::List {
                    element: Box::new(ResultShape::Column("c0".to_string())),
                    child: ChildRef {
                        stmt: 2,
                        parent: None,
                        cardinality: Cardinality::Many,
                        tag: None,
                    },
                },
                ResultShape::List {
                    element: Box::new(ResultShape::Column("c0".to_string())),
                    child: ChildRef {
                        stmt: 1,
                        parent: None,
                        cardinality: Cardinality::Many,
                        tag: None,
                    },
                },
            ])),
            child: ChildRef {
                stmt: 0,
                parent: None,
                cardinality: Cardinality::Many,
                tag: None,
            },
        }
    );
}

#[test]
fn lit_multi_list_record() {
    use crate::codegen::model::{Cardinality, ChildRef, ResultShape};
    let compiled = compile("[{ a = [1], b = [2] }]");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(&compiled.statements[0].sql, r#"SELECT ? AS "c0""#);
    assert_eq!(
        compiled.result,
        ResultShape::List {
            element: Box::new(ResultShape::Record(vec![
                (
                    "a".to_string(),
                    ResultShape::List {
                        element: Box::new(ResultShape::Column("c0".to_string())),
                        child: ChildRef {
                            stmt: 2,
                            parent: None,
                            cardinality: Cardinality::Many,
                            tag: None,
                        },
                    },
                ),
                (
                    "b".to_string(),
                    ResultShape::List {
                        element: Box::new(ResultShape::Column("c0".to_string())),
                        child: ChildRef {
                            stmt: 1,
                            parent: None,
                            cardinality: Cardinality::Many,
                            tag: None,
                        },
                    },
                ),
            ])),
            child: ChildRef {
                stmt: 0,
                parent: None,
                cardinality: Cardinality::Many,
                tag: None,
            },
        }
    );
}

#[test]
fn lit_flatten_record() {
    use crate::codegen::model::{Cardinality, ChildRef, ResultShape};
    let compiled = compile("[[{ a = 1 }]].flatten()");
    assert_eq!(compiled.statements.len(), 1);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"SELECT "lit1"."c1" AS "a" FROM "$$lit_2_II"('lit1') AS "lit1" WHERE ("lit1"."c0" IN (SELECT "lit0"."c0" FROM "$$lit_1_I"('lit0') AS "lit0"))"#,
    );
    assert_eq!(
        compiled.result,
        ResultShape::List {
            element: Box::new(ResultShape::Record(vec![(
                "a".to_string(),
                ResultShape::Column("a".to_string()),
            )])),
            child: ChildRef {
                stmt: 0,
                parent: None,
                cardinality: Cardinality::Many,
                tag: None,
            },
        }
    );
}

#[test]
fn if_mismatched_branch_nesting() {
    use crate::codegen::model::{Cardinality, ChildRef, ParentRef, ResultShape};
    let compiled = compile("if true { [[1]] } else { users.scores }");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"SELECT "lit0"."c0" FROM "$$lit_1_I"('lit0') AS "lit0""#,
    );
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "lit1".* FROM "$$lit_2_II"('lit1') AS "lit1" WHERE ("lit1"."c0" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"()))"#,
    );
    assert_eq!(
        compiled.result,
        ResultShape::List {
            element: Box::new(ResultShape::List {
                element: Box::new(ResultShape::Column("c1".to_string())),
                child: ChildRef {
                    stmt: 1,
                    parent: Some(ParentRef {
                        stmt: 0,
                        parent_key: "c0".to_string(),
                        child_key: "c0".to_string(),
                        negated: false,
                    }),
                    cardinality: Cardinality::Many,
                    tag: None,
                },
            }),
            child: ChildRef {
                stmt: 1,
                parent: Some(ParentRef {
                    stmt: 0,
                    parent_key: "c0".to_string(),
                    child_key: "c0".to_string(),
                    negated: false,
                }),
                cardinality: Cardinality::Many,
                tag: None,
            },
        }
    );
}

#[test]
fn if_sunk_branches() {
    use crate::codegen::model::{Cardinality, ChildRef, ParentRef, ResultShape};
    let compiled =
        compile("if true { users { name, o = orders } } else { users { name, o = orders } }");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"SELECT "$$users0"."name", "$$users0"."id" FROM "users" AS "$$users0""#,
    );
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$orders1".* FROM "orders" AS "$$orders1" WHERE ("$$orders1"."user_id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"()))"#,
    );
    let orders_list = ResultShape::List {
        element: Box::new(ResultShape::Record(vec![(
            "total".to_string(),
            ResultShape::Column("total".to_string()),
        )])),
        child: ChildRef {
            stmt: 1,
            parent: Some(ParentRef {
                stmt: 0,
                parent_key: "id".to_string(),
                child_key: "user_id".to_string(),
                negated: false,
            }),
            cardinality: Cardinality::Many,
            tag: None,
        },
    };
    assert_eq!(
        compiled.result,
        ResultShape::List {
            element: Box::new(ResultShape::Record(vec![
                ("name".to_string(), ResultShape::Column("name".to_string()),),
                ("o".to_string(), orders_list),
            ])),
            child: ChildRef {
                stmt: 0,
                parent: None,
                cardinality: Cardinality::Many,
                tag: None,
            },
        }
    );
}

#[test]
fn if_sunk_branches_global_cond() {
    use crate::codegen::model::{Cardinality, ChildRef, ParentRef, ResultShape};
    let compiled = compile(
        "if users.len() > 0 { users { name, o = orders } } else { users { name, o = orders } }",
    );
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"SELECT "$$users2"."name", "$$users2"."id" FROM "users" AS "$$users2" WHERE ((SELECT COUNT(*) AS "$$val1" FROM "users" AS "$$users0") > ?) UNION ALL SELECT "$$users4"."name", "$$users4"."id" FROM "users" AS "$$users4" WHERE NOT ((SELECT COUNT(*) AS "$$val1" FROM "users" AS "$$users0") > ?)"#,
    );
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$orders3".* FROM "orders" AS "$$orders3" WHERE ("$$orders3"."user_id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"())) AND ((SELECT COUNT(*) AS "$$val1" FROM "users" AS "$$users0") > ?) UNION ALL SELECT "$$orders5".* FROM "orders" AS "$$orders5" WHERE ("$$orders5"."user_id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"())) AND NOT ((SELECT COUNT(*) AS "$$val1" FROM "users" AS "$$users0") > ?)"#,
    );
    let orders_list = ResultShape::List {
        element: Box::new(ResultShape::Record(vec![(
            "total".to_string(),
            ResultShape::Column("total".to_string()),
        )])),
        child: ChildRef {
            stmt: 1,
            parent: Some(ParentRef {
                stmt: 0,
                parent_key: "id".to_string(),
                child_key: "user_id".to_string(),
                negated: false,
            }),
            cardinality: Cardinality::Many,
            tag: None,
        },
    };
    assert_eq!(
        compiled.result,
        ResultShape::List {
            element: Box::new(ResultShape::Record(vec![
                ("name".to_string(), ResultShape::Column("name".to_string()),),
                ("o".to_string(), orders_list),
            ])),
            child: ChildRef {
                stmt: 0,
                parent: None,
                cardinality: Cardinality::Many,
                tag: None,
            },
        }
    );
}

#[test]
fn lit_single_tuple_middle() {
    use crate::codegen::model::{Cardinality, ChildRef, ParentRef, ResultShape};
    let compiled = compile("[(1, [2])]");
    assert_eq!(compiled.statements.len(), 2);
    assert_eq!(
        compiled.result,
        ResultShape::List {
            element: Box::new(ResultShape::Tuple(vec![
                ResultShape::Column("c1".to_string()),
                ResultShape::List {
                    element: Box::new(ResultShape::Column("c1".to_string())),
                    child: ChildRef {
                        stmt: 1,
                        parent: Some(ParentRef {
                            stmt: 0,
                            parent_key: "c0".to_string(),
                            child_key: "c0".to_string(),
                            negated: false,
                        }),
                        cardinality: Cardinality::Many,
                        tag: None,
                    },
                },
            ])),
            child: ChildRef {
                stmt: 0,
                parent: None,
                cardinality: Cardinality::Many,
                tag: None,
            },
        }
    );
}

#[test]
fn if_middle_root_branches() {
    use crate::codegen::model::{Cardinality, ChildRef, ParentRef, ResultShape};
    let compiled = compile("if true { [(1, [2])] } else { [(3, [4])] }");
    assert_eq!(
        compiled.result,
        ResultShape::List {
            element: Box::new(ResultShape::Tuple(vec![
                ResultShape::Column("c1".to_string()),
                ResultShape::List {
                    element: Box::new(ResultShape::Column("c1".to_string())),
                    child: ChildRef {
                        stmt: 1,
                        parent: Some(ParentRef {
                            stmt: 0,
                            parent_key: "c0".to_string(),
                            child_key: "c0".to_string(),
                            negated: false,
                        }),
                        cardinality: Cardinality::Many,
                        tag: None,
                    },
                },
            ])),
            child: ChildRef {
                stmt: 0,
                parent: None,
                cardinality: Cardinality::Many,
                tag: None,
            },
        }
    );
}

#[test]
fn via_array_scalar_elements() {
    use crate::codegen::model::{Cardinality, ChildRef, ParentRef, ResultShape};
    let compiled = compile("users { sc = scores }");
    assert_eq!(
        compiled.result,
        ResultShape::List {
            element: Box::new(ResultShape::Record(vec![(
                "sc".to_string(),
                ResultShape::List {
                    element: Box::new(ResultShape::Column("score_value".to_string())),
                    child: ChildRef {
                        stmt: 1,
                        parent: Some(ParentRef {
                            stmt: 0,
                            parent_key: "id".to_string(),
                            child_key: "user_key".to_string(),
                            negated: false,
                        }),
                        cardinality: Cardinality::Many,
                        tag: None,
                    },
                }
            ),])),
            child: ChildRef {
                stmt: 0,
                parent: None,
                cardinality: Cardinality::Many,
                tag: None,
            },
        }
    );
}

#[test]
fn take_window_drops_helper_column() {
    use crate::codegen::model::{Cardinality, ChildRef, ParentRef, ResultShape};
    let compiled = compile("users { o = orders.take(2) { total } }");
    assert_eq!(
        compiled.result,
        ResultShape::List {
            element: Box::new(ResultShape::Record(vec![(
                "o".to_string(),
                ResultShape::List {
                    element: Box::new(ResultShape::Record(vec![(
                        "total".to_string(),
                        ResultShape::Column("total".to_string())
                    ),])),
                    child: ChildRef {
                        stmt: 1,
                        parent: Some(ParentRef {
                            stmt: 0,
                            parent_key: "id".to_string(),
                            child_key: "user_id".to_string(),
                            negated: false,
                        }),
                        cardinality: Cardinality::Many,
                        tag: None,
                    },
                }
            ),])),
            child: ChildRef {
                stmt: 0,
                parent: None,
                cardinality: Cardinality::Many,
                tag: None,
            },
        }
    );
}

#[test]
fn lit_record_element() {
    use crate::codegen::model::{Cardinality, ChildRef, ResultShape};
    let compiled = compile("[{ total = 42.0 }]");
    assert_eq!(compiled.statements.len(), 1);
    assert_eq!(
        compiled.result,
        ResultShape::List {
            element: Box::new(ResultShape::Record(vec![(
                "total".to_string(),
                ResultShape::Column("c0".to_string())
            ),])),
            child: ChildRef {
                stmt: 0,
                parent: None,
                cardinality: Cardinality::Many,
                tag: None,
            },
        }
    );
    let mut expected = HashMap::new();
    expected.insert(
        "lit0".to_string(),
        vec![vec![ParamValue::Dec(rust_decimal::Decimal::new(420, 1))]],
    );
    assert_eq!(compiled.literals, expected);
}

#[test]
fn lit_nested_record() {
    use crate::codegen::model::{Cardinality, ChildRef, ParentRef, ResultShape};
    let compiled = compile("[[{ a = 1 }]]");
    assert_eq!(compiled.statements.len(), 2);
    let child = ChildRef {
        stmt: 1,
        parent: Some(ParentRef {
            stmt: 0,
            parent_key: "c0".to_string(),
            child_key: "c0".to_string(),
            negated: false,
        }),
        cardinality: Cardinality::Many,
        tag: None,
    };
    assert_eq!(
        compiled.result,
        ResultShape::List {
            element: Box::new(ResultShape::List {
                element: Box::new(ResultShape::Record(vec![(
                    "a".to_string(),
                    ResultShape::Column("c1".to_string())
                ),])),
                child: child.clone(),
            }),
            child,
        }
    );
}

#[test]
fn lit_record_with_list() {
    use crate::codegen::model::{Cardinality, ChildRef, ParentRef, ResultShape};
    let compiled = compile("[[{ a = [1, 2] }]]");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "lit1".* FROM "$$lit_2_II"('lit1') AS "lit1" WHERE ("lit1"."c0" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"()))"#,
    );
    assert_eq!(
        compiled.result,
        ResultShape::List {
            element: Box::new(ResultShape::List {
                element: Box::new(ResultShape::Record(vec![(
                    "a".to_string(),
                    ResultShape::List {
                        element: Box::new(ResultShape::Column("c1".to_string())),
                        child: ChildRef {
                            stmt: 2,
                            parent: Some(ParentRef {
                                stmt: 1,
                                parent_key: "c1".to_string(),
                                child_key: "c0".to_string(),
                                negated: false,
                            }),
                            cardinality: Cardinality::Many,
                            tag: None,
                        },
                    }
                ),])),
                child: ChildRef {
                    stmt: 1,
                    parent: Some(ParentRef {
                        stmt: 0,
                        parent_key: "c0".to_string(),
                        child_key: "c0".to_string(),
                        negated: false,
                    }),
                    cardinality: Cardinality::Many,
                    tag: None,
                },
            }),
            child: ChildRef {
                stmt: 1,
                parent: Some(ParentRef {
                    stmt: 0,
                    parent_key: "c0".to_string(),
                    child_key: "c0".to_string(),
                    negated: false,
                }),
                cardinality: Cardinality::Many,
                tag: None,
            },
        }
    );
}

#[test]
fn lit_single_record_with_list() {
    use crate::codegen::model::{Cardinality, ChildRef, ParentRef, ResultShape};
    let compiled = compile("[{ a = [1] }]");
    assert_eq!(compiled.statements.len(), 2);
    assert_eq!(
        compiled.result,
        ResultShape::List {
            element: Box::new(ResultShape::Record(vec![(
                "a".to_string(),
                ResultShape::List {
                    element: Box::new(ResultShape::Column("c1".to_string())),
                    child: ChildRef {
                        stmt: 1,
                        parent: Some(ParentRef {
                            stmt: 0,
                            parent_key: "c0".to_string(),
                            child_key: "c0".to_string(),
                            negated: false,
                        }),
                        cardinality: Cardinality::Many,
                        tag: None,
                    },
                }
            ),])),
            child: ChildRef {
                stmt: 0,
                parent: None,
                cardinality: Cardinality::Many,
                tag: None,
            },
        }
    );
}

#[test]
fn lit_tuple_with_record_position() {
    use crate::codegen::model::{Cardinality, ChildRef, ResultShape};

    let compiled = compile("[(1, { x = 1 })]");
    assert_eq!(compiled.statements.len(), 1);
    assert_eq!(
        compiled.result,
        ResultShape::List {
            element: Box::new(ResultShape::Tuple(vec![
                ResultShape::Column("c0".to_string()),
                ResultShape::Record(vec![(
                    "x".to_string(),
                    ResultShape::Column("c1".to_string())
                ),]),
            ])),
            child: ChildRef {
                stmt: 0,
                parent: None,
                cardinality: Cardinality::Many,
                tag: None,
            },
        }
    );
}

#[test]
fn tuple_field_branch() {
    use crate::codegen::model::{Cardinality, ChildRef, ResultShape};
    let compiled = compile("users { t = if active { (1.0f, 2.0f) } else { pos } }");
    assert_eq!(compiled.statements.len(), 1);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"SELECT (CASE WHEN "$$users0"."active" THEN ? ELSE "$$users0"."lat" END) AS "t_c0", (CASE WHEN "$$users0"."active" THEN ? ELSE "$$users0"."lng" END) AS "t_c1" FROM "users" AS "$$users0""#,
    );
    assert_eq!(
        compiled.result,
        ResultShape::List {
            element: Box::new(ResultShape::Record(vec![(
                "t".to_string(),
                ResultShape::Tuple(vec![
                    ResultShape::Column("t_c0".to_string()),
                    ResultShape::Column("t_c1".to_string()),
                ]),
            )])),
            child: ChildRef {
                stmt: 0,
                parent: None,
                cardinality: Cardinality::Many,
                tag: None,
            },
        }
    );
}

#[test]
fn contains_global_field() {
    let (sql, params) = sql("users.filter(configs.name.contains(\"a\"))");
    assert_sql_eq(
        &sql,
        r#"SELECT "$$users0".* FROM "users" AS "$$users0" WHERE EXISTS (SELECT "$$subq2".* FROM (SELECT "$$configs1"."name" FROM "configs" AS "$$configs1") AS "$$subq2" WHERE ("$$subq2"."name" = ?))"#,
    );
    assert_eq!(params, vec![str_param("a")]);
}

#[test]
fn nested_struct_field() {
    use crate::codegen::model::{Cardinality, ChildRef, ParentRef, ResultShape};
    let compiled = compile("{ xs = [[1]] }");
    assert_eq!(compiled.statements.len(), 3);
    assert_eq!(
        compiled.result,
        ResultShape::Record(vec![(
            "xs".to_string(),
            ResultShape::List {
                element: Box::new(ResultShape::List {
                    element: Box::new(ResultShape::Column("c1".to_string())),
                    child: ChildRef {
                        stmt: 2,
                        parent: Some(ParentRef {
                            stmt: 1,
                            parent_key: "c0".to_string(),
                            child_key: "c0".to_string(),
                            negated: false,
                        }),
                        cardinality: Cardinality::Many,
                        tag: None,
                    },
                }),
                child: ChildRef {
                    stmt: 1,
                    parent: None,
                    cardinality: Cardinality::Many,
                    tag: None,
                },
            },
        )]),
    );
}

#[test]
fn optional_list_single_chain() {
    use crate::codegen::model::{Cardinality, ChildRef, ParentRef, ResultShape};
    let compiled = compile("users { x = mentor?.orders }");
    assert_eq!(compiled.statements.len(), 4);
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$split2"."$$key" AS "c0" FROM "$$split2" AS "$$split2" WHERE ("$$split2"."mentor_id" IS NOT ?)"#,
    );
    assert_sql_eq(
        &compiled.statements[3].sql,
        r#"SELECT "$$split2"."$$key" AS "c0", "$$orders1"."total", "$$orders1"."user_id", "$$orders1"."id" FROM "$$split2" AS "$$split2", "orders" AS "$$orders1" WHERE ("$$orders1"."user_id" = "$$split2"."mentor_id")"#,
    );
    assert_eq!(compiled.statements[3].params, Vec::<Param>::new());
    assert_eq!(
        compiled.result,
        ResultShape::List {
            element: Box::new(ResultShape::Record(vec![(
                "x".to_string(),
                ResultShape::Single {
                    element: Box::new(ResultShape::List {
                        element: Box::new(ResultShape::Record(vec![(
                            "total".to_string(),
                            ResultShape::Column("total".to_string()),
                        )])),
                        child: ChildRef {
                            stmt: 3,
                            parent: Some(ParentRef {
                                stmt: 2,
                                parent_key: "c0".to_string(),
                                child_key: "c0".to_string(),
                                negated: false,
                            }),
                            cardinality: Cardinality::Many,
                            tag: None,
                        },
                    }),
                    child: ChildRef {
                        stmt: 2,
                        parent: Some(ParentRef {
                            stmt: 0,
                            parent_key: "$$key".to_string(),
                            child_key: "c0".to_string(),
                            negated: false,
                        }),
                        cardinality: Cardinality::One,
                        tag: None,
                    },
                },
            )])),
            child: ChildRef {
                stmt: 1,
                parent: None,
                cardinality: Cardinality::Many,
                tag: None,
            },
        }
    );
}

#[test]
fn optional_list_temp_collapse() {
    use crate::codegen::model::{Cardinality, ChildRef, ParentRef, ResultShape};
    let compiled = compile("users { x = [[age]].first() }");
    assert_eq!(compiled.statements.len(), 4);
    assert_sql_eq(
        &compiled.statements[3].sql,
        r#"SELECT "$$split2"."$$key" AS "c0", "$$split2"."age" AS "c1" FROM "$$split2" AS "$$split2" WHERE ("$$split2"."c1" = ?)"#,
    );
    assert_eq!(
        compiled.result,
        ResultShape::List {
            element: Box::new(ResultShape::Record(vec![(
                "x".to_string(),
                ResultShape::Single {
                    element: Box::new(ResultShape::List {
                        element: Box::new(ResultShape::Column("c1".to_string())),
                        child: ChildRef {
                            stmt: 3,
                            parent: Some(ParentRef {
                                stmt: 1,
                                parent_key: "$$key".to_string(),
                                child_key: "c0".to_string(),
                                negated: false,
                            }),
                            cardinality: Cardinality::Many,
                            tag: None,
                        },
                    }),
                    child: ChildRef {
                        stmt: 1,
                        parent: Some(ParentRef {
                            stmt: 0,
                            parent_key: "$$key".to_string(),
                            child_key: "c0".to_string(),
                            negated: false,
                        }),
                        cardinality: Cardinality::One,
                        tag: None,
                    },
                },
            )])),
            child: ChildRef {
                stmt: 2,
                parent: None,
                cardinality: Cardinality::Many,
                tag: None,
            },
        }
    );
    let prefixed = compile("users { x = [[age]].take(1).first() }");
    assert_eq!(prefixed.statements.len(), 4);
}

#[test]
fn optional_list_item() {
    let compiled = compile("users { t = [[1]].first() }");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "lit1".* FROM "$$lit_1_I"('lit1') AS "lit1" LIMIT ?"#,
    );
    assert_eq!(compiled.statements[1].params, vec![int_param(1)]);
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "lit2".* FROM "$$lit_2_II"('lit2') AS "lit2" WHERE ("lit2"."c0" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"()))"#,
    );
}

#[test]
fn optional_list_tuple_element() {
    let compiled = compile("([1], [[1]].first())");
    assert_eq!(compiled.statements.len(), 4);
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "lit1".* FROM "$$lit_1_I"('lit1') AS "lit1" LIMIT ?"#,
    );
    assert_eq!(compiled.statements[1].params, vec![int_param(1)]);
}

#[test]
fn optional_list_item_storage() {
    let compiled = compile("users { o = orders.first()?.items }");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$subq3".* FROM (SELECT "$$orders2".*, "ROW_NUMBER"() OVER (PARTITION BY "$$orders2"."user_id") AS "$$rn" FROM "orders" AS "$$orders2" WHERE ("$$orders2"."user_id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"()))) AS "$$subq3" WHERE ("$$subq3"."$$rn" <= ?)"#,
    );
    assert_eq!(compiled.statements[1].params, vec![int_param(1)]);
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$items1".* FROM "items" AS "$$items1" WHERE ("$$items1"."order_id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"()))"#,
    );
}

#[test]
fn middle_scoped_literal_collapse() {
    let compiled = compile("users { o = orders { t = [[1]].first() } }");
    assert_eq!(compiled.statements.len(), 4);
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "lit2".* FROM "$$lit_2_I"('lit2') AS "lit2" LIMIT ?"#,
    );
    assert_sql_eq(
        &compiled.statements[3].sql,
        r#"SELECT "lit3".* FROM "$$lit_3_II"('lit3') AS "lit3" WHERE ("lit3"."c0" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"()))"#,
    );
}

#[test]
fn middle_scoped_storage_chain() {
    use crate::codegen::model::{Cardinality, ChildRef, ParentRef, ResultShape};
    let compiled = compile("users { o = subordinates { t = orders.first()?.items } }");
    assert_eq!(compiled.statements.len(), 4);
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$subq3".* FROM (SELECT "$$orders2".*, "ROW_NUMBER"() OVER (PARTITION BY "$$orders2"."user_id") AS "$$rn" FROM "orders" AS "$$orders2" WHERE ("$$orders2"."user_id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"()))) AS "$$subq3" WHERE ("$$subq3"."$$rn" <= ?)"#,
    );
    assert_sql_eq(
        &compiled.statements[3].sql,
        r#"SELECT "$$items1".* FROM "items" AS "$$items1" WHERE ("$$items1"."order_id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"()))"#,
    );
    assert_eq!(
        compiled.result,
        ResultShape::List {
            element: Box::new(ResultShape::Record(vec![(
                "o".to_string(),
                ResultShape::List {
                    element: Box::new(ResultShape::Record(vec![(
                        "t".to_string(),
                        ResultShape::Single {
                            element: Box::new(ResultShape::List {
                                element: Box::new(ResultShape::Record(vec![(
                                    "sku".to_string(),
                                    ResultShape::Column("sku".to_string()),
                                )])),
                                child: ChildRef {
                                    stmt: 3,
                                    parent: Some(ParentRef {
                                        stmt: 2,
                                        parent_key: "id".to_string(),
                                        child_key: "order_id".to_string(),
                                        negated: false,
                                    }),
                                    cardinality: Cardinality::Many,
                                    tag: None,
                                },
                            }),
                            child: ChildRef {
                                stmt: 2,
                                parent: Some(ParentRef {
                                    stmt: 1,
                                    parent_key: "id".to_string(),
                                    child_key: "user_id".to_string(),
                                    negated: false,
                                }),
                                cardinality: Cardinality::One,
                                tag: None,
                            },
                        },
                    )])),
                    child: ChildRef {
                        stmt: 1,
                        parent: Some(ParentRef {
                            stmt: 0,
                            parent_key: "id".to_string(),
                            child_key: "manager_id".to_string(),
                            negated: false,
                        }),
                        cardinality: Cardinality::Many,
                        tag: None,
                    },
                },
            )])),
            child: ChildRef {
                stmt: 0,
                parent: None,
                cardinality: Cardinality::Many,
                tag: None,
            },
        }
    );
}

#[test]
fn lit_record_field_access() {
    let (sql, params) = sql("[{x = 1}, {x = 2}].x");
    assert_sql_eq(
        &sql,
        r#"SELECT "lit0"."c0" AS "x" FROM "$$lit_1_I"('lit0') AS "lit0""#,
    );
    assert!(params.is_empty());
}

#[test]
fn per_parent_nav_needle() {
    let (sql, params) = sql("users { x = orders.contains(manager.orders.first().unwrap()) }");
    assert_sql_eq(
        &sql,
        r#"SELECT EXISTS (SELECT "$$orders2".* FROM (SELECT "$$subq6"."total", "$$subq6"."user_id", "$$subq6"."id" FROM (SELECT "$$subq4".*, ? AS "$$abort" FROM (SELECT "$$orders3".* FROM "orders" AS "$$orders3" WHERE ("$$orders3"."user_id" = "$$users0"."manager_id") LIMIT ?) AS "$$subq4" UNION ALL SELECT ? AS "c0", ? AS "c1", ? AS "c2", ? AS "$$abort" WHERE ((CASE WHEN NOT EXISTS (SELECT ? AS "c0" FROM (SELECT "$$subq4".* FROM (SELECT "$$orders3".* FROM "orders" AS "$$orders3" WHERE ("$$orders3"."user_id" = "$$users0"."manager_id") LIMIT ?) AS "$$subq4") AS "$$subq5") THEN "$$panic"(?) ELSE ? END) IS NOT ?)) AS "$$subq6") AS "$$subq7", "orders" AS "$$orders2" WHERE ("$$orders2"."user_id" = "$$users0"."id") AND ("$$orders2"."total" IS "$$subq7"."total") AND ("$$orders2"."user_id" IS "$$subq7"."user_id") AND ("$$orders2"."id" IS "$$subq7"."id")) AS "x" FROM "users" AS "$$users0""#,
    );
    assert_eq!(
        params,
        vec![
            Param::Value(ParamValue::Null),
            int_param(1),
            Param::Value(ParamValue::Null),
            Param::Value(ParamValue::Null),
            Param::Value(ParamValue::Null),
            Param::Value(ParamValue::Null),
            int_param(1),
            int_param(1),
            str_param("unwrap failed: value was none"),
            Param::Value(ParamValue::Null),
            Param::Value(ParamValue::Null),
        ]
    );
}

#[test]
fn lit_split_field_access_values() {
    let (sql, params) = sql("[{xs = [1], y = 2}].y");
    assert_sql_eq(
        &sql,
        r#"SELECT "lit0"."c1" AS "y" FROM "$$lit_2_II"('lit0') AS "lit0""#,
    );
    assert!(params.is_empty());
}

#[test]
#[should_panic(expected = "M6 query-valued array element")]
fn lit_query_valued_mix_defers() {
    sql("[{a = users.first(), y = 2}].y");
}

#[test]
fn single_hop_scalar_len() {
    let (sql, params) = sql("users { x = manager.orders.len() }");
    assert_sql_eq(
        &sql,
        r#"SELECT (SELECT COUNT(*) AS "$$val2" FROM "orders" AS "$$orders1" WHERE ("$$orders1"."user_id" = "$$users0"."manager_id")) AS "x" FROM "users" AS "$$users0""#,
    );
    assert!(params.is_empty());
}

#[test]
fn single_hop_take_scalar_len() {
    let (sql_take, params_take) = sql("users { x = manager.orders.take(1).len() }");
    assert_sql_eq(
        &sql_take,
        r#"SELECT (SELECT COUNT(*) AS "$$val3" FROM (SELECT "$$orders1".* FROM "orders" AS "$$orders1" WHERE ("$$orders1"."user_id" = "$$users0"."manager_id") LIMIT (CASE WHEN ? >= ? THEN ? ELSE "$$panic"(?) END)) AS "$$subq2") AS "x" FROM "users" AS "$$users0""#,
    );
    assert_eq!(
        params_take,
        vec![
            int_param(1),
            int_param(0),
            int_param(1),
            str_param("take requires a non-negative integer"),
        ]
    );
    let (sql_skip, params_skip) = sql("users { x = manager.orders.skip(1).len() }");
    assert_sql_eq(
        &sql_skip,
        r#"SELECT (SELECT COUNT(*) AS "$$val3" FROM (SELECT "$$orders1".* FROM "orders" AS "$$orders1" WHERE ("$$orders1"."user_id" = "$$users0"."manager_id") LIMIT -1 OFFSET (CASE WHEN ? >= ? THEN ? ELSE "$$panic"(?) END)) AS "$$subq2") AS "x" FROM "users" AS "$$users0""#,
    );
    assert_eq!(
        params_skip,
        vec![
            int_param(1),
            int_param(0),
            int_param(1),
            str_param("skip requires a non-negative integer"),
        ]
    );
}

#[test]
fn multi_hop_scalar_len() {
    let (sql_take, params_take) = sql("users { n = manager.manager.orders.len() }");
    assert_sql_eq(
        &sql_take,
        r#"SELECT (SELECT COUNT(*) AS "$$val6" FROM "orders" AS "$$orders1" WHERE ("$$orders1"."user_id" = (SELECT (SELECT "$$users3"."id" AS "$$val4" FROM "users" AS "$$users3" WHERE ("$$users3"."id" = "$$users2"."manager_id")) AS "$$val5" FROM "users" AS "$$users2" WHERE ("$$users2"."id" = "$$users0"."manager_id")))) AS "n" FROM "users" AS "$$users0""#,
    );
    assert!(params_take.is_empty());
}

#[test]
fn single_hop_take_bound() {
    let compiled = compile("users { x = manager.orders.take(age) }");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$subq5"."total", "$$subq5"."user_id", "$$subq5"."id", "$$subq5"."$$pkey" FROM (SELECT "$$orders4"."total", "$$orders4"."user_id", "$$orders4"."id", "$$$$split23"."$$key" AS "$$pkey", "$$$$split23"."age" AS "$$bnd", "ROW_NUMBER"() OVER (PARTITION BY "$$$$split23"."$$key") AS "$$rn" FROM "$$split2" AS "$$$$split23", "orders" AS "$$orders4" WHERE ("$$orders4"."user_id" = "$$$$split23"."manager_id")) AS "$$subq5" WHERE ("$$subq5"."$$rn" <= "$$subq5"."$$bnd")"#,
    );
    assert!(compiled.statements[2].params.is_empty());
    use crate::codegen::model::{Cardinality, ChildRef, ParentRef, ResultShape};
    let middle = ResultShape::List {
        element: Box::new(ResultShape::Record(vec![(
            "total".to_string(),
            ResultShape::Column("total".to_string()),
        )])),
        child: ChildRef {
            stmt: 2,
            parent: Some(ParentRef {
                stmt: 0,
                parent_key: "$$key".to_string(),
                child_key: "$$pkey".to_string(),
                negated: false,
            }),
            cardinality: Cardinality::Many,
            tag: None,
        },
    };
    assert_eq!(
        compiled.result,
        ResultShape::List {
            element: Box::new(ResultShape::Record(vec![("x".to_string(), middle,)])),
            child: ChildRef {
                stmt: 1,
                parent: None,
                cardinality: Cardinality::Many,
                tag: None,
            },
        }
    );
}

#[test]
fn single_hop_take_computed_bound() {
    let compiled = compile("users { x = manager.orders.take(age - 30) }");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"CREATE TEMP TABLE "$$split2" AS SELECT "$$users0".*, "$$sub"("$$users0"."age", ?) AS "$$bndc", "ROW_NUMBER"() OVER () AS "$$key" FROM "users" AS "$$users0""#,
    );
    assert_eq!(compiled.statements[0].params, vec![int_param(30)]);
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$subq5"."total", "$$subq5"."user_id", "$$subq5"."id", "$$subq5"."$$pkey" FROM (SELECT "$$orders4"."total", "$$orders4"."user_id", "$$orders4"."id", "$$$$split23"."$$key" AS "$$pkey", "$$$$split23"."$$bndc" AS "$$bnd", "ROW_NUMBER"() OVER (PARTITION BY "$$$$split23"."$$key") AS "$$rn" FROM "$$split2" AS "$$$$split23", "orders" AS "$$orders4" WHERE ("$$orders4"."user_id" = "$$$$split23"."manager_id")) AS "$$subq5" WHERE ("$$subq5"."$$rn" <= "$$subq5"."$$bnd")"#,
    );
    assert!(compiled.statements[2].params.is_empty());
}

#[test]
fn subquery_needle_contains() {
    let (sql, params) = sql("users { x = scores.contains(manager.age) }");
    assert_sql_eq(
        &sql,
        r#"SELECT EXISTS (SELECT "$$user_scores1"."user_key" FROM "user_scores" AS "$$user_scores1" WHERE ("$$user_scores1"."user_key" = "$$users0"."id") AND ("$$user_scores1"."score_value" = (SELECT "$$users2"."age" FROM "users" AS "$$users2" WHERE ("$$users2"."id" = "$$users0"."manager_id")))) AS "x" FROM "users" AS "$$users0""#,
    );
    assert!(params.is_empty());
}

#[test]
fn take_bound_contains() {
    let (sql, params) = sql("users { x = tags.take(age).contains(\"x\") }");
    assert_sql_eq(
        &sql,
        r#"SELECT EXISTS (SELECT "$$subq2"."user_key" FROM (SELECT "$$user_tags1"."user_key", "$$user_tags1"."tag_value", "ROW_NUMBER"() OVER (PARTITION BY "$$user_tags1"."user_key") AS "$$rn" FROM "user_tags" AS "$$user_tags1") AS "$$subq2" WHERE ("$$subq2"."user_key" = "$$users0"."id") AND ("$$subq2"."$$rn" <= "$$users0"."age") AND ("$$subq2"."tag_value" = ?)) AS "x" FROM "users" AS "$$users0""#,
    );
    assert_eq!(params, vec![str_param("x")]);
}

#[test]
fn per_parent_contains() {
    let (sql, params) = sql("users { x = orders.contains(orders.first().unwrap()) }");
    assert_sql_eq(
        &sql,
        r#"SELECT EXISTS (SELECT "$$orders2".* FROM (SELECT "$$subq6"."total", "$$subq6"."user_id", "$$subq6"."id" FROM (SELECT "$$subq4".*, ? AS "$$abort" FROM (SELECT "$$orders3".* FROM "orders" AS "$$orders3" WHERE ("$$orders3"."user_id" = "$$users0"."id") LIMIT ?) AS "$$subq4" UNION ALL SELECT ? AS "c0", ? AS "c1", ? AS "c2", ? AS "$$abort" WHERE ((CASE WHEN NOT EXISTS (SELECT ? AS "c0" FROM (SELECT "$$subq4".* FROM (SELECT "$$orders3".* FROM "orders" AS "$$orders3" WHERE ("$$orders3"."user_id" = "$$users0"."id") LIMIT ?) AS "$$subq4") AS "$$subq5") THEN "$$panic"(?) ELSE ? END) IS NOT ?)) AS "$$subq6") AS "$$subq7", "orders" AS "$$orders2" WHERE ("$$orders2"."user_id" = "$$users0"."id") AND ("$$orders2"."total" IS "$$subq7"."total") AND ("$$orders2"."user_id" IS "$$subq7"."user_id") AND ("$$orders2"."id" IS "$$subq7"."id")) AS "x" FROM "users" AS "$$users0""#,
    );
    assert_eq!(
        params,
        vec![
            Param::Value(ParamValue::Null),
            int_param(1),
            Param::Value(ParamValue::Null),
            Param::Value(ParamValue::Null),
            Param::Value(ParamValue::Null),
            Param::Value(ParamValue::Null),
            int_param(1),
            int_param(1),
            str_param("unwrap failed: value was none"),
            Param::Value(ParamValue::Null),
            Param::Value(ParamValue::Null),
        ]
    );
}

#[test]
fn nested_collapse_scalar_tail_values() {
    let compiled = compile("users { x = subordinates.orders.first()?.total }");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$subq3".* FROM (SELECT "$$users1".*, "ROW_NUMBER"() OVER (PARTITION BY "$$users1"."manager_id") AS "$$rn" FROM "users" AS "$$users1" WHERE ("$$users1"."manager_id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"()))) AS "$$subq3" WHERE ("$$subq3"."$$rn" <= ?)"#,
    );
    assert_eq!(compiled.statements[1].params, vec![int_param(1)]);
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$orders2".* FROM "orders" AS "$$orders2" WHERE ("$$orders2"."user_id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"()))"#,
    );
    assert!(compiled.statements[2].params.is_empty());
}

#[test]
fn nested_collapse_presence_chain() {
    use crate::codegen::model::{Cardinality, ChildRef, ParentRef, ResultShape};
    let compiled = compile("users { o = subordinates.orders.first()?.items }");
    assert_eq!(compiled.statements.len(), 4);
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$subq4".* FROM (SELECT "$$users2".*, "ROW_NUMBER"() OVER (PARTITION BY "$$users2"."manager_id") AS "$$rn" FROM "users" AS "$$users2" WHERE ("$$users2"."manager_id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"()))) AS "$$subq4" WHERE ("$$subq4"."$$rn" <= ?)"#,
    );
    assert_eq!(compiled.statements[1].params, vec![int_param(1)]);
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$orders3".* FROM "orders" AS "$$orders3" WHERE ("$$orders3"."user_id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"()))"#,
    );
    assert!(compiled.statements[2].params.is_empty());
    assert_sql_eq(
        &compiled.statements[3].sql,
        r#"SELECT "$$items1".* FROM "items" AS "$$items1" WHERE ("$$items1"."order_id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"()))"#,
    );
    assert!(compiled.statements[3].params.is_empty());
    assert_eq!(
        compiled.result,
        ResultShape::List {
            element: Box::new(ResultShape::Record(vec![(
                "o".to_string(),
                ResultShape::Single {
                    element: Box::new(ResultShape::List {
                        element: Box::new(ResultShape::List {
                            element: Box::new(ResultShape::Record(vec![(
                                "sku".to_string(),
                                ResultShape::Column("sku".to_string()),
                            )])),
                            child: ChildRef {
                                stmt: 3,
                                parent: Some(ParentRef {
                                    stmt: 2,
                                    parent_key: "id".to_string(),
                                    child_key: "order_id".to_string(),
                                    negated: false,
                                }),
                                cardinality: Cardinality::Many,
                                tag: None,
                            },
                        }),
                        child: ChildRef {
                            stmt: 2,
                            parent: Some(ParentRef {
                                stmt: 1,
                                parent_key: "id".to_string(),
                                child_key: "user_id".to_string(),
                                negated: false,
                            }),
                            cardinality: Cardinality::Many,
                            tag: None,
                        },
                    }),
                    child: ChildRef {
                        stmt: 1,
                        parent: Some(ParentRef {
                            stmt: 0,
                            parent_key: "id".to_string(),
                            child_key: "manager_id".to_string(),
                            negated: false,
                        }),
                        cardinality: Cardinality::One,
                        tag: None,
                    },
                },
            )])),
            child: ChildRef {
                stmt: 0,
                parent: None,
                cardinality: Cardinality::Many,
                tag: None,
            },
        }
    );
}

#[test]
fn m2m_scalar_chain() {
    use crate::codegen::model::{Cardinality, ChildRef, ResultShape};
    let compiled = compile("users { a = roles.first()?.name }");
    assert_eq!(compiled.statements.len(), 1);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"SELECT (SELECT "$$roles2"."name" FROM "user_roles" AS "$$user_roles1", "roles" AS "$$roles2" WHERE ("$$user_roles1"."role_id" = "$$roles2"."id") AND ("$$user_roles1"."user_id" = "$$users0"."id")) AS "a" FROM "users" AS "$$users0""#,
    );
    assert!(compiled.statements[0].params.is_empty());
    assert_eq!(
        compiled.result,
        ResultShape::List {
            element: Box::new(ResultShape::Record(vec![(
                "a".to_string(),
                ResultShape::Column("a".to_string()),
            )])),
            child: ChildRef {
                stmt: 0,
                parent: None,
                cardinality: Cardinality::Many,
                tag: None,
            },
        }
    );
}

#[test]
fn m2m_nested_chain() {
    use crate::codegen::model::{Cardinality, ChildRef, ParentRef, ResultShape};
    let compiled = compile("users { r = roles.users }");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$roles2"."name", "$$roles2"."id", "$$user_roles1"."user_id" FROM "user_roles" AS "$$user_roles1", "roles" AS "$$roles2" WHERE ("$$user_roles1"."user_id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"())) AND ("$$roles2"."id" = "$$user_roles1"."role_id")"#,
    );
    assert!(compiled.statements[1].params.is_empty());
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$subq5".* FROM (SELECT "$$user_roles3"."role_id", "$$users4"."name", "$$users4"."address", "$$users4"."age", "$$users4"."score", "$$users4"."balance", "$$users4"."active", "$$users4"."nickname", "$$users4"."lat", "$$users4"."lng", "$$users4"."t0", "$$users4"."t1", "$$users4"."t2", "$$users4"."created", "$$users4"."ttl", "$$users4"."id", "$$users4"."manager_id", "$$users4"."mentor_id" FROM "user_roles" AS "$$user_roles3", "users" AS "$$users4" WHERE ("$$user_roles3"."user_id" = "$$users4"."id")) AS "$$subq5" WHERE ("$$subq5"."role_id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"()))"#,
    );
    assert!(compiled.statements[2].params.is_empty());
    assert_eq!(
        compiled.result,
        ResultShape::List {
            element: Box::new(ResultShape::Record(vec![(
                "r".to_string(),
                ResultShape::List {
                    element: Box::new(ResultShape::List {
                        element: Box::new(ResultShape::Record(vec![
                            ("name".to_string(), ResultShape::Column("name".to_string())),
                            (
                                "address".to_string(),
                                ResultShape::Column("address".to_string())
                            ),
                            ("age".to_string(), ResultShape::Column("age".to_string())),
                            (
                                "score".to_string(),
                                ResultShape::Column("score".to_string())
                            ),
                            (
                                "balance".to_string(),
                                ResultShape::Column("balance".to_string())
                            ),
                            (
                                "active".to_string(),
                                ResultShape::Column("active".to_string())
                            ),
                            (
                                "nickname".to_string(),
                                ResultShape::Column("nickname".to_string())
                            ),
                            ("lat".to_string(), ResultShape::Column("lat".to_string())),
                            ("lng".to_string(), ResultShape::Column("lng".to_string())),
                            ("t0".to_string(), ResultShape::Column("t0".to_string())),
                            ("t1".to_string(), ResultShape::Column("t1".to_string())),
                            ("t2".to_string(), ResultShape::Column("t2".to_string())),
                            (
                                "created".to_string(),
                                ResultShape::Column("created".to_string())
                            ),
                            ("ttl".to_string(), ResultShape::Column("ttl".to_string())),
                        ])),
                        child: ChildRef {
                            stmt: 2,
                            parent: Some(ParentRef {
                                stmt: 1,
                                parent_key: "id".to_string(),
                                child_key: "role_id".to_string(),
                                negated: false,
                            }),
                            cardinality: Cardinality::Many,
                            tag: None,
                        },
                    }),
                    child: ChildRef {
                        stmt: 1,
                        parent: Some(ParentRef {
                            stmt: 0,
                            parent_key: "id".to_string(),
                            child_key: "user_id".to_string(),
                            negated: false,
                        }),
                        cardinality: Cardinality::Many,
                        tag: None,
                    },
                },
            )])),
            child: ChildRef {
                stmt: 0,
                parent: None,
                cardinality: Cardinality::Many,
                tag: None,
            },
        }
    );
}

#[test]
fn m2m_double_chain() {
    use crate::codegen::model::{Cardinality, ChildRef, ParentRef, ResultShape};
    let compiled = compile("users { r = roles.users }");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$roles2"."name", "$$roles2"."id", "$$user_roles1"."user_id" FROM "user_roles" AS "$$user_roles1", "roles" AS "$$roles2" WHERE ("$$user_roles1"."user_id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"())) AND ("$$roles2"."id" = "$$user_roles1"."role_id")"#,
    );
    assert!(compiled.statements[1].params.is_empty());
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$subq5".* FROM (SELECT "$$user_roles3"."role_id", "$$users4"."name", "$$users4"."address", "$$users4"."age", "$$users4"."score", "$$users4"."balance", "$$users4"."active", "$$users4"."nickname", "$$users4"."lat", "$$users4"."lng", "$$users4"."t0", "$$users4"."t1", "$$users4"."t2", "$$users4"."created", "$$users4"."ttl", "$$users4"."id", "$$users4"."manager_id", "$$users4"."mentor_id" FROM "user_roles" AS "$$user_roles3", "users" AS "$$users4" WHERE ("$$user_roles3"."user_id" = "$$users4"."id")) AS "$$subq5" WHERE ("$$subq5"."role_id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"()))"#,
    );
    assert!(compiled.statements[2].params.is_empty());
    let user_record = ResultShape::Record(vec![
        ("name".to_string(), ResultShape::Column("name".to_string())),
        (
            "address".to_string(),
            ResultShape::Column("address".to_string()),
        ),
        ("age".to_string(), ResultShape::Column("age".to_string())),
        (
            "score".to_string(),
            ResultShape::Column("score".to_string()),
        ),
        (
            "balance".to_string(),
            ResultShape::Column("balance".to_string()),
        ),
        (
            "active".to_string(),
            ResultShape::Column("active".to_string()),
        ),
        (
            "nickname".to_string(),
            ResultShape::Column("nickname".to_string()),
        ),
        ("lat".to_string(), ResultShape::Column("lat".to_string())),
        ("lng".to_string(), ResultShape::Column("lng".to_string())),
        ("t0".to_string(), ResultShape::Column("t0".to_string())),
        ("t1".to_string(), ResultShape::Column("t1".to_string())),
        ("t2".to_string(), ResultShape::Column("t2".to_string())),
        (
            "created".to_string(),
            ResultShape::Column("created".to_string()),
        ),
        ("ttl".to_string(), ResultShape::Column("ttl".to_string())),
    ]);
    assert_eq!(
        compiled.result,
        ResultShape::List {
            element: Box::new(ResultShape::Record(vec![(
                "r".to_string(),
                ResultShape::List {
                    element: Box::new(ResultShape::List {
                        element: Box::new(user_record),
                        child: ChildRef {
                            stmt: 2,
                            parent: Some(ParentRef {
                                stmt: 1,
                                parent_key: "id".to_string(),
                                child_key: "role_id".to_string(),
                                negated: false,
                            }),
                            cardinality: Cardinality::Many,
                            tag: None,
                        },
                    }),
                    child: ChildRef {
                        stmt: 1,
                        parent: Some(ParentRef {
                            stmt: 0,
                            parent_key: "id".to_string(),
                            child_key: "user_id".to_string(),
                            negated: false,
                        }),
                        cardinality: Cardinality::Many,
                        tag: None,
                    },
                },
            )])),
            child: ChildRef {
                stmt: 0,
                parent: None,
                cardinality: Cardinality::Many,
                tag: None,
            },
        }
    );
}

#[test]
fn m2m_reanchored_inner() {
    use crate::codegen::model::{Cardinality, ChildRef, ParentRef, ResultShape};
    let compiled = compile("users { o = orders { r = roles } }");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$subq4"."user_id" FROM (SELECT "$$orders1"."user_id" FROM "orders" AS "$$orders1" WHERE ("$$orders1"."user_id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"()))) AS "$$subq4""#,
    );
    assert!(compiled.statements[1].params.is_empty());
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$roles3"."name", "$$roles3"."id", "$$user_roles2"."user_id" FROM "user_roles" AS "$$user_roles2", "roles" AS "$$roles3" WHERE ("$$user_roles2"."user_id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"())) AND ("$$roles3"."id" = "$$user_roles2"."role_id")"#,
    );
    assert!(compiled.statements[2].params.is_empty());
    let role_list = ResultShape::List {
        element: Box::new(ResultShape::Record(vec![(
            "name".to_string(),
            ResultShape::Column("name".to_string()),
        )])),
        child: ChildRef {
            stmt: 2,
            parent: Some(ParentRef {
                stmt: 1,
                parent_key: "user_id".to_string(),
                child_key: "user_id".to_string(),
                negated: false,
            }),
            cardinality: Cardinality::Many,
            tag: None,
        },
    };
    assert_eq!(
        compiled.result,
        ResultShape::List {
            element: Box::new(ResultShape::Record(vec![(
                "o".to_string(),
                ResultShape::List {
                    element: Box::new(ResultShape::Record(vec![("r".to_string(), role_list,)])),
                    child: ChildRef {
                        stmt: 1,
                        parent: Some(ParentRef {
                            stmt: 0,
                            parent_key: "id".to_string(),
                            child_key: "user_id".to_string(),
                            negated: false,
                        }),
                        cardinality: Cardinality::Many,
                        tag: None,
                    },
                },
            )])),
            child: ChildRef {
                stmt: 0,
                parent: None,
                cardinality: Cardinality::Many,
                tag: None,
            },
        }
    );
}

#[test]
fn m2m_pairs_through_junction() {
    let compiled = compile("users { r = roles }");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$roles2"."name", "$$roles2"."id", "$$user_roles1"."user_id" FROM "user_roles" AS "$$user_roles1", "roles" AS "$$roles2" WHERE ("$$user_roles1"."user_id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"())) AND ("$$roles2"."id" = "$$user_roles1"."role_id")"#,
    );
    assert!(compiled.statements.iter().all(|s| s.params.is_empty()));
}

#[test]
fn m2m_take_windows_junction_key() {
    let compiled = compile("users { r = roles.take(1) }");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$subq4".* FROM (SELECT "$$subq3".*, "ROW_NUMBER"() OVER (PARTITION BY "$$subq3"."user_id") AS "$$rn" FROM (SELECT "$$roles2"."name", "$$roles2"."id", "$$user_roles1"."user_id" FROM "user_roles" AS "$$user_roles1", "roles" AS "$$roles2" WHERE ("$$user_roles1"."user_id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"())) AND ("$$roles2"."id" = "$$user_roles1"."role_id")) AS "$$subq3") AS "$$subq4" WHERE ("$$subq4"."$$rn" <= (CASE WHEN ? >= ? THEN ? ELSE "$$panic"(?) END))"#,
    );
    assert_eq!(
        compiled.statements[1].params,
        vec![int_param(1), int_param(0), int_param(1), limit_msg("take")]
    );
}

#[test]
fn m2m_skip_windows_junction_key() {
    let compiled = compile("users { r = roles.skip(1) }");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$subq4".* FROM (SELECT "$$subq3".*, "ROW_NUMBER"() OVER (PARTITION BY "$$subq3"."user_id") AS "$$rn" FROM (SELECT "$$roles2"."name", "$$roles2"."id", "$$user_roles1"."user_id" FROM "user_roles" AS "$$user_roles1", "roles" AS "$$roles2" WHERE ("$$user_roles1"."user_id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"())) AND ("$$roles2"."id" = "$$user_roles1"."role_id")) AS "$$subq3") AS "$$subq4" WHERE ("$$subq4"."$$rn" > (CASE WHEN ? >= ? THEN ? ELSE "$$panic"(?) END))"#,
    );
    assert_eq!(
        compiled.statements[1].params,
        vec![int_param(1), int_param(0), int_param(1), limit_msg("skip")]
    );
}

const SHADOW_SCHEMA_SRC: &str = r#"
    root users: User;
    root roles: Role;
    struct User {
        name: String,
        roles: &List<Role>,
    }
    struct Role {
        name: String,
        owner: Int@user_id,
        users: &List<User>,
    }
    rel Role.users <<->> User.roles via user_roles[role_id -> roles.id, user_id -> users.id];
"#;

fn compile_shadow(query: &str) -> CompiledQuery {
    let schema = grove_schema::validate(
        grove_schema::parse_schema(SHADOW_SCHEMA_SRC)
            .0
            .expect("invalid test schema"),
    )
    .0
    .expect("invalid test schema");
    let (file, parse_diags) = crate::parse_query(query);
    assert!(
        parse_diags.is_empty(),
        "expected no parse errors, got {parse_diags:?}"
    );
    let (typed, type_diags) = typecheck(file.unwrap(), &schema);
    assert!(
        type_diags.is_empty(),
        "expected no type errors, got {type_diags:?}"
    );
    crate::codegen::codegen(&typed.unwrap(), &schema)
}

#[test]
fn m2m_shadowed_take_windows_alias() {
    let compiled = compile_shadow("users { r = roles.take(1) }");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$subq4".* FROM (SELECT "$$subq3".*, "ROW_NUMBER"() OVER (PARTITION BY "$$subq3"."$$j_user_id") AS "$$rn" FROM (SELECT "$$roles2"."name", "$$roles2"."user_id", "$$roles2"."id", "$$user_roles1"."user_id" AS "$$j_user_id" FROM "user_roles" AS "$$user_roles1", "roles" AS "$$roles2" WHERE ("$$user_roles1"."user_id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"())) AND ("$$roles2"."id" = "$$user_roles1"."role_id")) AS "$$subq3") AS "$$subq4" WHERE ("$$subq4"."$$rn" <= (CASE WHEN ? >= ? THEN ? ELSE "$$panic"(?) END))"#,
    );
    assert_eq!(
        compiled.statements[1].params,
        vec![int_param(1), int_param(0), int_param(1), limit_msg("take")]
    );
}

#[test]
fn m2m_shadowed_take_projected_key() {
    let compiled = compile_shadow("users { r = roles.take(1) { name } }");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$subq4"."name", "$$subq4"."$$j_user_id" FROM (SELECT "$$subq3".*, "ROW_NUMBER"() OVER (PARTITION BY "$$subq3"."$$j_user_id") AS "$$rn" FROM (SELECT "$$roles2"."name", "$$roles2"."user_id", "$$roles2"."id", "$$user_roles1"."user_id" AS "$$j_user_id" FROM "user_roles" AS "$$user_roles1", "roles" AS "$$roles2" WHERE ("$$user_roles1"."user_id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"())) AND ("$$roles2"."id" = "$$user_roles1"."role_id")) AS "$$subq3") AS "$$subq4" WHERE ("$$subq4"."$$rn" <= (CASE WHEN ? >= ? THEN ? ELSE "$$panic"(?) END))"#,
    );
    assert_eq!(
        compiled.statements[1].params,
        vec![int_param(1), int_param(0), int_param(1), limit_msg("take")]
    );
}

#[test]
fn m2m_shadowed_nested_scalar_tail() {
    let compiled = compile_shadow("users { x = roles.users.first()?.name }");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$subq7".* FROM (SELECT "$$subq3".*, "ROW_NUMBER"() OVER (PARTITION BY "$$subq3"."$$j_user_id") AS "$$rn" FROM (SELECT "$$roles2"."name", "$$roles2"."user_id", "$$roles2"."id", "$$user_roles1"."user_id" AS "$$j_user_id" FROM "user_roles" AS "$$user_roles1", "roles" AS "$$roles2" WHERE ("$$user_roles1"."user_id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"())) AND ("$$roles2"."id" = "$$user_roles1"."role_id")) AS "$$subq3") AS "$$subq7" WHERE ("$$subq7"."$$rn" <= ?)"#,
    );
    assert_eq!(compiled.statements[1].params, vec![int_param(1)]);
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$subq6".* FROM (SELECT "$$user_roles4"."role_id", "$$users5"."name", "$$users5"."id" FROM "user_roles" AS "$$user_roles4", "users" AS "$$users5" WHERE ("$$user_roles4"."user_id" = "$$users5"."id")) AS "$$subq6" WHERE ("$$subq6"."role_id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"()))"#,
    );
    assert!(compiled.statements[2].params.is_empty());
}

#[test]
fn m2m_shadowed_single_first_pairs() {
    use crate::codegen::model::ResultShape;
    let compiled = compile_shadow("users { r = roles.first() }");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$subq3".* FROM (SELECT "$$roles2"."name", "$$roles2"."user_id", "$$roles2"."id", "$$user_roles1"."user_id" AS "$$j_user_id" FROM "user_roles" AS "$$user_roles1", "roles" AS "$$roles2" WHERE ("$$user_roles1"."user_id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"())) AND ("$$roles2"."id" = "$$user_roles1"."role_id")) AS "$$subq3""#,
    );
    assert!(compiled.statements[1].params.is_empty());
    let ResultShape::List { element, .. } = &compiled.result else {
        panic!("root result shape drifted");
    };
    let ResultShape::Record(fields) = element.as_ref() else {
        panic!("element result shape drifted");
    };
    let Some((_, ResultShape::Single { child, .. })) = fields.iter().find(|(name, _)| name == "r")
    else {
        panic!("single result shape drifted");
    };
    let parent = child.parent.as_ref().expect("unpaired single");
    assert_eq!(parent.child_key, "$$j_user_id");
}

#[test]
fn m2m_single_first_pairs() {
    use crate::codegen::model::{Cardinality, ChildRef, ParentRef, ResultShape};
    let compiled = compile("users { r = roles.first() }");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$subq3".* FROM (SELECT "$$roles2"."name", "$$roles2"."id", "$$user_roles1"."user_id" FROM "user_roles" AS "$$user_roles1", "roles" AS "$$roles2" WHERE ("$$user_roles1"."user_id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"())) AND ("$$roles2"."id" = "$$user_roles1"."role_id")) AS "$$subq3""#,
    );
    assert!(compiled.statements[1].params.is_empty());
    assert_eq!(
        compiled.result,
        ResultShape::List {
            element: Box::new(ResultShape::Record(vec![(
                "r".to_string(),
                ResultShape::Single {
                    element: Box::new(ResultShape::Record(vec![(
                        "name".to_string(),
                        ResultShape::Column("name".to_string()),
                    )])),
                    child: ChildRef {
                        stmt: 1,
                        parent: Some(ParentRef {
                            stmt: 0,
                            parent_key: "id".to_string(),
                            child_key: "user_id".to_string(),
                            negated: false,
                        }),
                        cardinality: Cardinality::One,
                        tag: None,
                    },
                },
            )])),
            child: ChildRef {
                stmt: 0,
                parent: None,
                cardinality: Cardinality::Many,
                tag: None,
            },
        }
    );
}

#[test]
fn m2m_maxby_windows_junction_key() {
    let compiled = compile("users { r = roles.max_by(name) }");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$subq4".* FROM (SELECT "$$subq3".*, "ROW_NUMBER"() OVER (PARTITION BY "$$subq3"."user_id" ORDER BY "$$subq3"."name" DESC) AS "$$rn" FROM (SELECT "$$roles2"."name", "$$roles2"."id", "$$user_roles1"."user_id" FROM "user_roles" AS "$$user_roles1", "roles" AS "$$roles2" WHERE ("$$user_roles1"."user_id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"())) AND ("$$roles2"."id" = "$$user_roles1"."role_id")) AS "$$subq3") AS "$$subq4" WHERE ("$$subq4"."$$rn" <= ?)"#,
    );
    assert_eq!(compiled.statements[1].params, vec![int_param(1)]);
}

#[test]
fn m2m_nth_windows_junction_key() {
    let compiled = compile("users { r = roles.nth(0) }");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$subq4".* FROM (SELECT "$$subq3".*, "ROW_NUMBER"() OVER (PARTITION BY "$$subq3"."user_id") AS "$$rn" FROM (SELECT "$$roles2"."name", "$$roles2"."id", "$$user_roles1"."user_id" FROM "user_roles" AS "$$user_roles1", "roles" AS "$$roles2" WHERE ("$$user_roles1"."user_id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"())) AND ("$$roles2"."id" = "$$user_roles1"."role_id")) AS "$$subq3") AS "$$subq4" WHERE ("$$subq4"."$$rn" = ?)"#,
    );
    assert_eq!(compiled.statements[1].params, vec![int_param(1)]);
}

#[test]
fn m2m_corr_projection_len() {
    let (sql, params) = sql("users { s = roles { name }.len() }");
    assert_sql_eq(
        &sql,
        r#"SELECT (SELECT COUNT(*) AS "$$val6" FROM (SELECT "$$roles4"."name" FROM "roles" AS "$$roles4" WHERE ("$$roles4"."id" IN (SELECT "$$user_roles3"."role_id" FROM "user_roles" AS "$$user_roles3" WHERE ("$$user_roles3"."user_id" = "$$users0"."id")))) AS "$$subq5") AS "s" FROM "users" AS "$$users0""#,
    );
    assert!(params.is_empty());
}

#[test]
fn m2m_projection_serves_key() {
    let compiled = compile("roles { u = users { n = name } }");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$users2"."name" AS "n", "$$user_roles1"."role_id" FROM "user_roles" AS "$$user_roles1", "users" AS "$$users2" WHERE ("$$user_roles1"."role_id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"())) AND ("$$users2"."id" = "$$user_roles1"."user_id")"#,
    );
    assert!(compiled.statements.iter().all(|s| s.params.is_empty()));
}

#[test]
fn nested_list_item() {
    use crate::codegen::model::{Cardinality, ChildRef, ParentRef, ResultShape};
    let compiled = compile("users { o = orders { i = items } }");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"SELECT "$$users0"."id" FROM "users" AS "$$users0""#,
    );
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$orders1"."id", "$$orders1"."user_id" FROM "orders" AS "$$orders1" WHERE ("$$orders1"."user_id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"()))"#,
    );
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$items2".* FROM "items" AS "$$items2" WHERE ("$$items2"."order_id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"()))"#,
    );
    assert_eq!(
        compiled.result,
        ResultShape::List {
            element: Box::new(ResultShape::Record(vec![(
                "o".to_string(),
                ResultShape::List {
                    element: Box::new(ResultShape::Record(vec![(
                        "i".to_string(),
                        ResultShape::List {
                            element: Box::new(ResultShape::Record(vec![(
                                "sku".to_string(),
                                ResultShape::Column("sku".to_string()),
                            )])),
                            child: ChildRef {
                                stmt: 2,
                                parent: Some(ParentRef {
                                    stmt: 1,
                                    parent_key: "id".to_string(),
                                    child_key: "order_id".to_string(),
                                    negated: false,
                                }),
                                cardinality: Cardinality::Many,
                                tag: None,
                            },
                        },
                    )])),
                    child: ChildRef {
                        stmt: 1,
                        parent: Some(ParentRef {
                            stmt: 0,
                            parent_key: "id".to_string(),
                            child_key: "user_id".to_string(),
                            negated: false,
                        }),
                        cardinality: Cardinality::Many,
                        tag: None,
                    },
                },
            )])),
            child: ChildRef {
                stmt: 0,
                parent: None,
                cardinality: Cardinality::Many,
                tag: None,
            },
        }
    );
}

#[test]
fn nested_record_item() {
    use crate::codegen::model::{Cardinality, ChildRef, ParentRef, ResultShape};
    let compiled = compile("users { s = subordinates { m = manager } }");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"SELECT "$$users0"."id" FROM "users" AS "$$users0""#,
    );
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$users1"."manager_id" FROM "users" AS "$$users1" WHERE ("$$users1"."manager_id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"()))"#,
    );
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$users2".* FROM "users" AS "$$users2" WHERE ("$$users2"."id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"()))"#,
    );
    let ResultShape::List {
        element,
        child: top,
    } = &compiled.result
    else {
        panic!("expected top list, got {:?}", compiled.result);
    };
    assert_eq!(top.stmt, 0);
    assert!(top.parent.is_none());
    let ResultShape::Record(fields) = element.as_ref() else {
        panic!("expected top record, got {element:?}");
    };
    let (_, s_shape) = fields.iter().find(|(n, _)| n == "s").unwrap();
    let ResultShape::List { child: s_child, .. } = s_shape else {
        panic!("expected s list, got {s_shape:?}");
    };
    assert_eq!(
        s_child,
        &ChildRef {
            stmt: 1,
            parent: Some(ParentRef {
                stmt: 0,
                parent_key: "id".to_string(),
                child_key: "manager_id".to_string(),
                negated: false,
            }),
            cardinality: Cardinality::Many,
            tag: None,
        }
    );
}

#[test]
fn nested_sorted_middle() {
    let compiled = compile("users { o = orders.sort(total) { i = items } }");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$orders1"."id", "$$orders1"."user_id" FROM "orders" AS "$$orders1" WHERE ("$$orders1"."user_id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"())) ORDER BY "$$orders1"."total" ASC"#,
    );
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$items2".* FROM "items" AS "$$items2" WHERE ("$$items2"."order_id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"()))"#,
    );
}

#[test]
fn nested_single_item() {
    use crate::codegen::model::{Cardinality, ChildRef, ParentRef, ResultShape};
    let compiled = compile("users { o = orders { c = items.first() } }");
    assert_eq!(compiled.statements.len(), 3);
    let ResultShape::List { element, .. } = &compiled.result else {
        panic!("expected top list, got {:?}", compiled.result);
    };
    let ResultShape::Record(fields) = element.as_ref() else {
        panic!("expected top record, got {element:?}");
    };
    let (_, o_shape) = fields.iter().find(|(n, _)| n == "o").unwrap();
    let ResultShape::List { element: o_el, .. } = o_shape else {
        panic!("expected o list, got {o_shape:?}");
    };
    let ResultShape::Record(o_fields) = o_el.as_ref() else {
        panic!("expected o record, got {o_el:?}");
    };
    let (_, c_shape) = o_fields.iter().find(|(n, _)| n == "c").unwrap();
    let ResultShape::Single { child: c_child, .. } = c_shape else {
        panic!("expected c single, got {c_shape:?}");
    };
    assert_eq!(
        c_child,
        &ChildRef {
            stmt: 2,
            parent: Some(ParentRef {
                stmt: 1,
                parent_key: "id".to_string(),
                child_key: "order_id".to_string(),
                negated: false,
            }),
            cardinality: Cardinality::One,
            tag: None,
        }
    );
}

#[test]
fn nested_depth_cap() {
    let compiled = compile("users { o = orders { i = items { w = order } } }");
    assert_eq!(compiled.statements.len(), 4);
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$items2"."order_id" FROM "items" AS "$$items2" WHERE ("$$items2"."order_id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"()))"#,
    );
    assert_sql_eq(
        &compiled.statements[3].sql,
        r#"SELECT "$$orders3".* FROM "orders" AS "$$orders3" WHERE ("$$orders3"."id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"()))"#,
    );
}

#[test]
fn nested_taken_middle() {
    let compiled = compile("users { o = orders.take(2) { i = items } }");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$subq2"."id", "$$subq2"."user_id" FROM (SELECT "$$orders1".*, "ROW_NUMBER"() OVER (PARTITION BY "$$orders1"."user_id") AS "$$rn" FROM "orders" AS "$$orders1" WHERE ("$$orders1"."user_id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"()))) AS "$$subq2" WHERE ("$$subq2"."$$rn" <= (CASE WHEN ? >= ? THEN ? ELSE "$$panic"(?) END))"#,
    );
    assert_eq!(
        compiled.statements[1].params,
        vec![
            int_param(2),
            int_param(0),
            int_param(2),
            str_param("take requires a non-negative integer"),
        ]
    );
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$items3".* FROM "items" AS "$$items3" WHERE ("$$items3"."order_id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"()))"#,
    );
}

#[test]
fn nested_depth_four() {
    let compiled = compile("users { o = orders { i = items { w = order { v = items } } } }");
    assert_eq!(compiled.statements.len(), 5);
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$items2"."order_id" FROM "items" AS "$$items2" WHERE ("$$items2"."order_id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"()))"#,
    );
    assert_sql_eq(
        &compiled.statements[3].sql,
        r#"SELECT "$$orders3"."id" FROM "orders" AS "$$orders3" WHERE ("$$orders3"."id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"()))"#,
    );
    assert_sql_eq(
        &compiled.statements[4].sql,
        r#"SELECT "$$items4".* FROM "items" AS "$$items4" WHERE ("$$items4"."order_id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"()))"#,
    );
}

#[test]
fn schema_tuple_nested() {
    use crate::codegen::model::{Cardinality, ChildRef, ResultShape};
    let compiled = compile("users { t = trio }");
    assert_eq!(compiled.statements.len(), 1);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"SELECT "$$users0"."t0" AS "t_c0", "$$users0"."t1" AS "t_c1_c0", "$$users0"."t2" AS "t_c1_c1" FROM "users" AS "$$users0""#,
    );
    assert_eq!(
        compiled.result,
        ResultShape::List {
            element: Box::new(ResultShape::Record(vec![(
                "t".to_string(),
                ResultShape::Tuple(vec![
                    ResultShape::Column("t_c0".to_string()),
                    ResultShape::Tuple(vec![
                        ResultShape::Column("t_c1_c0".to_string()),
                        ResultShape::Column("t_c1_c1".to_string()),
                    ]),
                ]),
            )])),
            child: ChildRef {
                stmt: 0,
                parent: None,
                cardinality: Cardinality::Many,
                tag: None,
            },
        }
    );
}

#[test]
fn sort_sum_derived() {
    let (sql, params) = sql("users { s = [3, 1, 2].sort().sum() }");
    assert_sql_eq(
        &sql,
        r#"SELECT (SELECT "SUM"("lit1"."c0") AS "$$val2" FROM "$$lit_1_I"('lit1') AS "lit1") AS "s" FROM "users" AS "$$users0""#,
    );
    assert!(params.is_empty());
}

#[test]
fn agg_nav_base() {
    let (sql, params) = sql("users { s = orders.total.sum() }");
    assert_sql_eq(
        &sql,
        r#"SELECT (SELECT "$$sum_dec"("$$subq2"."total") AS "$$val3" FROM (SELECT "$$orders1"."total" FROM "orders" AS "$$orders1" WHERE ("$$orders1"."user_id" = "$$users0"."id")) AS "$$subq2") AS "s" FROM "users" AS "$$users0""#,
    );
    assert!(params.is_empty());
}

#[test]
fn unwrap_nav_field_correlated() {
    let compiled = compile("users { m = mentor.unwrap().name }");
    assert_eq!(compiled.statements.len(), 1);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"SELECT (SELECT "$$subq4"."name" FROM (SELECT "$$subq2".*, ? AS "$$abort" FROM (SELECT "$$users1".* FROM "users" AS "$$users1" WHERE ("$$users1"."id" = "$$users0"."mentor_id")) AS "$$subq2" UNION ALL SELECT ? AS "c0", ? AS "c1", ? AS "c2", ? AS "c3", ? AS "c4", ? AS "c5", ? AS "c6", ? AS "c7", ? AS "c8", ? AS "c9", ? AS "c10", ? AS "c11", ? AS "c12", ? AS "c13", ? AS "c14", ? AS "c15", ? AS "c16", ? AS "$$abort" WHERE ((CASE WHEN NOT EXISTS (SELECT ? AS "c0" FROM (SELECT "$$subq2".* FROM (SELECT "$$users1".* FROM "users" AS "$$users1" WHERE ("$$users1"."id" = "$$users0"."mentor_id")) AS "$$subq2") AS "$$subq3") THEN "$$panic"(?) ELSE ? END) IS NOT ?)) AS "$$subq4") AS "m" FROM "users" AS "$$users0""#,
    );
}

#[test]
fn unwrap_record_result() {
    let compiled = compile("users.first().unwrap()");
    assert_eq!(compiled.statements.len(), 1);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"SELECT "$$subq3".* FROM (SELECT "$$subq1".*, ? AS "$$abort" FROM (SELECT "$$users0".* FROM "users" AS "$$users0" LIMIT ?) AS "$$subq1" UNION ALL SELECT ? AS "c0", ? AS "c1", ? AS "c2", ? AS "c3", ? AS "c4", ? AS "c5", ? AS "c6", ? AS "c7", ? AS "c8", ? AS "c9", ? AS "c10", ? AS "c11", ? AS "c12", ? AS "c13", ? AS "c14", ? AS "c15", ? AS "c16", ? AS "$$abort" WHERE ((CASE WHEN NOT EXISTS (SELECT ? AS "c0" FROM (SELECT "$$subq1".* FROM (SELECT "$$users0".* FROM "users" AS "$$users0" LIMIT ?) AS "$$subq1") AS "$$subq2") THEN "$$panic"(?) ELSE ? END) IS NOT ?)) AS "$$subq3""#,
    );
    assert_eq!(
        compiled.statements[0].params,
        vec![
            Param::Value(ParamValue::Null),
            int_param(1),
            Param::Value(ParamValue::Null),
            Param::Value(ParamValue::Null),
            Param::Value(ParamValue::Null),
            Param::Value(ParamValue::Null),
            Param::Value(ParamValue::Null),
            Param::Value(ParamValue::Null),
            Param::Value(ParamValue::Null),
            Param::Value(ParamValue::Null),
            Param::Value(ParamValue::Null),
            Param::Value(ParamValue::Null),
            Param::Value(ParamValue::Null),
            Param::Value(ParamValue::Null),
            Param::Value(ParamValue::Null),
            Param::Value(ParamValue::Null),
            Param::Value(ParamValue::Null),
            Param::Value(ParamValue::Null),
            Param::Value(ParamValue::Null),
            Param::Value(ParamValue::Null),
            int_param(1),
            int_param(1),
            str_param("unwrap failed: value was none"),
            Param::Value(ParamValue::Null),
            Param::Value(ParamValue::Null),
        ]
    );
}

#[test]
fn unwrap_or_record_literal_values() {
    let (sql, params) = sql("[{a = 1}].first().unwrap_or({a = 0})");
    assert_sql_eq(
        &sql,
        r#"SELECT "$$subq4".* FROM (SELECT "$$subq1".* FROM (SELECT "lit0"."c0" AS "a" FROM "$$lit_1_I"('lit0') AS "lit0" LIMIT ?) AS "$$subq1" UNION ALL SELECT "$$subq2".* FROM (SELECT ? AS "a") AS "$$subq2" WHERE NOT EXISTS (SELECT ? AS "c0" FROM (SELECT "$$subq1".* FROM (SELECT "lit0"."c0" AS "a" FROM "$$lit_1_I"('lit0') AS "lit0" LIMIT ?) AS "$$subq1") AS "$$subq3")) AS "$$subq4""#,
    );
    assert_eq!(
        params,
        vec![int_param(1), int_param(0), int_param(1), int_param(1)]
    );
    let compiled = compile("users { x = [{a = 1}].first().unwrap_or({a = 0}) }");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$subq5".* FROM (SELECT "$$subq2".* FROM (SELECT "lit1"."c0" AS "a" FROM "$$lit_1_I"('lit1') AS "lit1" LIMIT ?) AS "$$subq2" UNION ALL SELECT "$$subq3".* FROM (SELECT ? AS "a") AS "$$subq3" WHERE NOT EXISTS (SELECT ? AS "c0" FROM (SELECT "$$subq2".* FROM (SELECT "lit1"."c0" AS "a" FROM "$$lit_1_I"('lit1') AS "lit1" LIMIT ?) AS "$$subq2") AS "$$subq4")) AS "$$subq5""#,
    );
    assert_eq!(
        compiled.statements[1].params,
        vec![int_param(1), int_param(0), int_param(1), int_param(1)]
    );
}

#[test]
fn unwrap_or_record_literal_take_prefix() {
    let (sql_take, params_take) = sql("[{a = 1}].take(1).first().unwrap_or({a = 0})");
    assert_sql_eq(
        &sql_take,
        r#"SELECT "$$subq4".* FROM (SELECT "$$subq1".* FROM (SELECT "lit0"."c0" AS "a" FROM "$$lit_1_I"('lit0') AS "lit0" LIMIT "min"(?, CASE WHEN ? >= ? THEN ? ELSE "$$panic"(?) END)) AS "$$subq1" UNION ALL SELECT "$$subq2".* FROM (SELECT ? AS "a") AS "$$subq2" WHERE NOT EXISTS (SELECT ? AS "c0" FROM (SELECT "$$subq1".* FROM (SELECT "lit0"."c0" AS "a" FROM "$$lit_1_I"('lit0') AS "lit0" LIMIT "min"(?, CASE WHEN ? >= ? THEN ? ELSE "$$panic"(?) END)) AS "$$subq1") AS "$$subq3")) AS "$$subq4""#,
    );
    assert_eq!(
        params_take,
        vec![
            int_param(1),
            int_param(1),
            int_param(0),
            int_param(1),
            str_param("take requires a non-negative integer"),
            int_param(0),
            int_param(1),
            int_param(1),
            int_param(1),
            int_param(0),
            int_param(1),
            str_param("take requires a non-negative integer"),
        ]
    );
    let compiled = compile("users { x = [{a = 1}].take(1).first().unwrap_or({a = 0}) }");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$subq5".* FROM (SELECT "$$subq2".* FROM (SELECT "lit1"."c0" AS "a" FROM "$$lit_1_I"('lit1') AS "lit1" LIMIT "min"(?, CASE WHEN ? >= ? THEN ? ELSE "$$panic"(?) END)) AS "$$subq2" UNION ALL SELECT "$$subq3".* FROM (SELECT ? AS "a") AS "$$subq3" WHERE NOT EXISTS (SELECT ? AS "c0" FROM (SELECT "$$subq2".* FROM (SELECT "lit1"."c0" AS "a" FROM "$$lit_1_I"('lit1') AS "lit1" LIMIT "min"(?, CASE WHEN ? >= ? THEN ? ELSE "$$panic"(?) END)) AS "$$subq2") AS "$$subq4")) AS "$$subq5""#,
    );
    assert_eq!(
        compiled.statements[1].params,
        vec![
            int_param(1),
            int_param(1),
            int_param(0),
            int_param(1),
            str_param("take requires a non-negative integer"),
            int_param(0),
            int_param(1),
            int_param(1),
            int_param(1),
            int_param(0),
            int_param(1),
            str_param("take requires a non-negative integer"),
        ]
    );
    let (sql_zero, params_zero) = sql("[{a = 1}].take(0).first().unwrap_or({a = 0})");
    assert_sql_eq(
        &sql_zero,
        r#"SELECT "$$subq4".* FROM (SELECT "$$subq1".* FROM (SELECT "lit0"."c0" AS "a" FROM "$$lit_1_I"('lit0') AS "lit0" LIMIT "min"(?, CASE WHEN ? >= ? THEN ? ELSE "$$panic"(?) END)) AS "$$subq1" UNION ALL SELECT "$$subq2".* FROM (SELECT ? AS "a") AS "$$subq2" WHERE NOT EXISTS (SELECT ? AS "c0" FROM (SELECT "$$subq1".* FROM (SELECT "lit0"."c0" AS "a" FROM "$$lit_1_I"('lit0') AS "lit0" LIMIT "min"(?, CASE WHEN ? >= ? THEN ? ELSE "$$panic"(?) END)) AS "$$subq1") AS "$$subq3")) AS "$$subq4""#,
    );
    assert_eq!(
        params_zero,
        vec![
            int_param(1),
            int_param(0),
            int_param(0),
            int_param(0),
            str_param("take requires a non-negative integer"),
            int_param(0),
            int_param(1),
            int_param(1),
            int_param(0),
            int_param(0),
            int_param(0),
            str_param("take requires a non-negative integer"),
        ]
    );
    let (sql_skip, params_skip) = sql("[{a = 1}, {a = 2}].skip(1).first().unwrap_or({a = 0})");
    assert_sql_eq(
        &sql_skip,
        r#"SELECT "$$subq4".* FROM (SELECT "$$subq1".* FROM (SELECT "lit0"."c0" AS "a" FROM "$$lit_1_I"('lit0') AS "lit0" LIMIT ? OFFSET (CASE WHEN ? >= ? THEN ? ELSE "$$panic"(?) END)) AS "$$subq1" UNION ALL SELECT "$$subq2".* FROM (SELECT ? AS "a") AS "$$subq2" WHERE NOT EXISTS (SELECT ? AS "c0" FROM (SELECT "$$subq1".* FROM (SELECT "lit0"."c0" AS "a" FROM "$$lit_1_I"('lit0') AS "lit0" LIMIT ? OFFSET (CASE WHEN ? >= ? THEN ? ELSE "$$panic"(?) END)) AS "$$subq1") AS "$$subq3")) AS "$$subq4""#,
    );
    assert_eq!(
        params_skip,
        vec![
            int_param(1),
            int_param(1),
            int_param(0),
            int_param(1),
            str_param("skip requires a non-negative integer"),
            int_param(0),
            int_param(1),
            int_param(1),
            int_param(1),
            int_param(0),
            int_param(1),
            str_param("skip requires a non-negative integer"),
        ]
    );
}

#[test]
fn unwrap_or_rows_falls_back() {
    let compiled = compile("users { o = [[1]].first().unwrap_or([2]) }");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$subq6".* FROM (SELECT "$$subq5".* FROM (SELECT "lit3".* FROM "$$lit_1_I"('lit3') AS "lit3") AS "$$subq5" UNION ALL SELECT "lit6".* FROM "$$lit_1_I"('lit6') AS "lit6" WHERE NOT EXISTS (SELECT ? AS "c0" FROM (SELECT "lit1".* FROM "$$lit_1_I"('lit1') AS "lit1" LIMIT ?) AS "$$subq4")) AS "$$subq6""#,
    );
}

#[test]
fn unwrap_or_negative_abort() {
    let compiled = compile("[[1]].nth(-1).unwrap_or([0])");
    assert_eq!(compiled.statements.len(), 1);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"SELECT "$$subq4".* FROM (SELECT "$$subq2".*, ? AS "$$abort" FROM (SELECT ? AS "c0" WHERE ?) AS "$$subq2" UNION ALL SELECT ? AS "c0", ? AS "$$abort" WHERE ((CASE WHEN NOT EXISTS (SELECT ? AS "c0" FROM (SELECT "$$subq2".* FROM (SELECT ? AS "c0" WHERE ?) AS "$$subq2") AS "$$subq3") THEN "$$panic"(?) ELSE ? END) IS NOT ?)) AS "$$subq4""#,
    );
}

#[test]
fn unwrap_or_tuple_pick() {
    let compiled = compile("[[(1, 2)]].first().unwrap_or([(0, 0)])");
    assert_eq!(compiled.statements.len(), 1);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"SELECT "$$subq6".* FROM (SELECT "$$subq4".* FROM (SELECT "lit2".* FROM "$$lit_2_II"('lit2') AS "lit2") AS "$$subq4" UNION ALL SELECT "lit5".* FROM "$$lit_2_II"('lit5') AS "lit5" WHERE NOT EXISTS (SELECT ? AS "c0" FROM (SELECT "lit0".* FROM "$$lit_1_I"('lit0') AS "lit0" LIMIT ?) AS "$$subq3")) AS "$$subq6""#,
    );
}

#[test]
fn unwrap_or_temp_presence() {
    let compiled = compile("users { x = [[age]].first().unwrap_or([0]) }");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$split1"."$$key" AS "c0", "$$split1"."age" AS "c1" FROM "$$split1" AS "$$split1" UNION ALL SELECT "$$split1"."$$key" AS "c0", ? AS "c1" FROM "$$split1" AS "$$split1" WHERE NOT EXISTS (SELECT ? AS "one" FROM (SELECT "$$split1"."$$key" AS "c0", "$$split1"."age" AS "c1" FROM "$$split1" AS "$$split1") AS "$$subq2" WHERE ("$$subq2"."c0" = "$$split1"."$$key"))"#,
    );
}

#[test]
fn unwrap_or_record_struct_default() {
    let compiled =
        compile("users { x = configs.take(1).first().unwrap_or({name = \"d\", owner = \"e\"}) }");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$subq5".* FROM (SELECT "$$subq2".* FROM (SELECT "$$configs1".* FROM "configs" AS "$$configs1" LIMIT "min"(?, CASE WHEN ? >= ? THEN ? ELSE "$$panic"(?) END)) AS "$$subq2" UNION ALL SELECT "$$subq3".* FROM (SELECT ? AS "name", ? AS "owner") AS "$$subq3" WHERE NOT EXISTS (SELECT ? AS "c0" FROM (SELECT "$$subq2".* FROM (SELECT "$$configs1".* FROM "configs" AS "$$configs1" LIMIT "min"(?, CASE WHEN ? >= ? THEN ? ELSE "$$panic"(?) END)) AS "$$subq2") AS "$$subq4")) AS "$$subq5""#,
    );
}

#[test]
fn unwrap_or_rows_folds_empty_pick() {
    let compiled = compile("users { o = [[1]].nth(5).unwrap_or([9]) }");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "lit3".* FROM "$$lit_1_I"('lit3') AS "lit3""#,
    );
}

#[test]
fn unwrap_list_result() {
    let compiled = compile("[[1]].first().unwrap()");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"SELECT "$$subq4"."c0" FROM (SELECT "$$subq2".*, ? AS "$$abort" FROM (SELECT "lit0".* FROM "$$lit_1_I"('lit0') AS "lit0" LIMIT ?) AS "$$subq2" UNION ALL SELECT ? AS "c0", ? AS "$$abort" WHERE ((CASE WHEN NOT EXISTS (SELECT ? AS "c0" FROM (SELECT "$$subq2".* FROM (SELECT "lit0".* FROM "$$lit_1_I"('lit0') AS "lit0" LIMIT ?) AS "$$subq2") AS "$$subq3") THEN "$$panic"(?) ELSE ? END) IS NOT ?)) AS "$$subq4""#,
    );
    assert_eq!(
        compiled.statements[0].params,
        vec![
            Param::Value(ParamValue::Null),
            int_param(1),
            Param::Value(ParamValue::Null),
            Param::Value(ParamValue::Null),
            int_param(1),
            int_param(1),
            str_param("unwrap failed: value was none"),
            Param::Value(ParamValue::Null),
            Param::Value(ParamValue::Null),
        ]
    );
}

#[test]
fn list_optional_mechanics() {
    let (sql, params) = sql("users.filter(users.orders.first().is_some())");
    assert_sql_eq(
        &sql,
        r#"SELECT "$$users0".* FROM "users" AS "$$users0" WHERE EXISTS (SELECT "$$users1".* FROM "users" AS "$$users1")"#,
    );
    assert!(params.is_empty());
}

#[test]
fn contains_derived() {
    let (sql, params) = sql("[[1, 2], [3]].flatten().contains(2)");
    assert_sql_eq(
        &sql,
        r#"SELECT EXISTS (SELECT "$$subq3".* FROM (SELECT "lit2"."c1" AS "c0" FROM "$$lit_2_II"('lit2') AS "lit2" WHERE ("lit2"."c0" IN (SELECT "lit1"."c0" FROM "$$lit_1_I"('lit1') AS "lit1"))) AS "$$subq3" WHERE ("$$subq3"."c0" = ?)) AS "$$val0""#,
    );
    assert_eq!(params, vec![int_param(2)]);
}

#[test]
fn contains_sorted() {
    let (sql, params) = sql("[3, 1, 2].sort().contains(2)");
    assert_sql_eq(
        &sql,
        r#"SELECT EXISTS (SELECT "lit1".* FROM "$$lit_1_I"('lit1') AS "lit1" WHERE ("lit1"."c0" = ?)) AS "$$val0""#,
    );
    assert_eq!(params, vec![int_param(2)]);
}

#[test]
fn scalar_sort_storage_values() {
    let compiled = compile("users { x = scores.sort() }");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$user_scores1".* FROM "user_scores" AS "$$user_scores1" WHERE ("$$user_scores1"."user_key" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"())) ORDER BY "$$user_scores1"."score_value" ASC"#,
    );
    assert!(compiled.statements[1].params.is_empty());
    let compiled = compile("users { x = scores.sort_desc() }");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$user_scores1".* FROM "user_scores" AS "$$user_scores1" WHERE ("$$user_scores1"."user_key" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"())) ORDER BY "$$user_scores1"."score_value" DESC"#,
    );
    assert!(compiled.statements[1].params.is_empty());
}

#[test]
fn scalar_sort_literal_reorder() {
    let (sql, params) = sql("[3, 1, 2].sort_desc()");
    assert_sql_eq(
        &sql,
        r#"SELECT "lit0".* FROM "$$lit_1_I"('lit0') AS "lit0""#,
    );
    assert!(params.is_empty());
}

#[test]
fn flatten_sum() {
    let (sql, params) = sql("[[1], [2]].flatten().sum()");
    assert_sql_eq(
        &sql,
        r#"SELECT "SUM"("$$subq3"."c0") AS "$$val0" FROM (SELECT "lit2"."c1" AS "c0" FROM "$$lit_2_II"('lit2') AS "lit2" WHERE ("lit2"."c0" IN (SELECT "lit1"."c0" FROM "$$lit_1_I"('lit1') AS "lit1"))) AS "$$subq3""#,
    );
    assert!(params.is_empty());
}

#[test]
fn flatten_nav_via_len() {
    let (sql, params) = sql("users { x = subordinates.scores.flatten().len() }");
    assert_sql_eq(
        &sql,
        r#"SELECT (SELECT COUNT(*) AS "$$val4" FROM (SELECT "$$user_scores2"."score_value" FROM "user_scores" AS "$$user_scores2" WHERE ("$$user_scores2"."user_key" IN (SELECT "$$users1"."id" FROM "users" AS "$$users1" WHERE ("$$users1"."manager_id" = "$$users0"."id")))) AS "$$subq3") AS "x" FROM "users" AS "$$users0""#,
    );
    assert!(params.is_empty());
}

#[test]
fn flatten_nav_via_sum() {
    let (sql, params) = sql("users { x = subordinates.scores.flatten().sum() }");
    assert_sql_eq(
        &sql,
        r#"SELECT (SELECT "SUM"("$$subq3"."score_value") AS "$$val4" FROM (SELECT "$$user_scores2"."score_value" FROM "user_scores" AS "$$user_scores2" WHERE ("$$user_scores2"."user_key" IN (SELECT "$$users1"."id" FROM "users" AS "$$users1" WHERE ("$$users1"."manager_id" = "$$users0"."id")))) AS "$$subq3") AS "x" FROM "users" AS "$$users0""#,
    );
    assert!(params.is_empty());
}

#[test]
fn tuple_needle_miss() {
    let (sql1, params1) = sql("[(1, 2)].contains((3, 4))");
    assert_sql_eq(&sql1, r#"SELECT ((? = ?) AND (? = ?)) AS "$$val0""#);
    assert_eq!(
        params1,
        vec![int_param(1), int_param(3), int_param(2), int_param(4)]
    );
}

#[test]
fn struct_beta_scalar() {
    let (sql, params) = sql("users { x = { n = name }.n }");
    assert_sql_eq(
        &sql,
        r#"SELECT "$$users0"."name" AS "x" FROM "users" AS "$$users0""#,
    );
    assert!(params.is_empty());
}

#[test]
fn struct_beta_chain() {
    let (sql, params) = sql("users { x = { m = manager }.m.name }");
    assert_sql_eq(
        &sql,
        r#"SELECT (SELECT "$$users1"."name" FROM "users" AS "$$users1" WHERE ("$$users1"."id" = "$$users0"."manager_id")) AS "x" FROM "users" AS "$$users0""#,
    );
    assert!(params.is_empty());
}

#[test]
fn struct_beta_rows() {
    let compiled = compile("{ o = [1, 2] }.o");
    assert_eq!(compiled.statements.len(), 1);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"SELECT "lit0".* FROM "$$lit_1_I"('lit0') AS "lit0""#,
    );
}

#[test]
fn struct_beta_tail() {
    let compiled = compile("users { o = { x = orders }.x.take(1) }");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"SELECT "$$users0"."id" FROM "users" AS "$$users0""#,
    );
}

#[test]
fn cross_sort_pure_outer() {
    let compiled = compile("users { x = configs.sort(email) }");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$configs2"."name", "$$configs2"."owner", "$$split1"."$$key" AS "$$pkey" FROM "$$split1" AS "$$split1", "configs" AS "$$configs2" ORDER BY "$$split1"."address" ASC"#,
    );
    assert!(compiled.statements[2].params.is_empty());
}

#[test]
fn window_pending() {
    use crate::codegen::model::{Cardinality, ChildRef, ParentRef, ResultShape};
    let compiled = compile("users { o = subordinates.orders.take(1) }");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$subq3".* FROM (SELECT "$$users1".*, "ROW_NUMBER"() OVER (PARTITION BY "$$users1"."manager_id") AS "$$rn" FROM "users" AS "$$users1" WHERE ("$$users1"."manager_id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"()))) AS "$$subq3" WHERE ("$$subq3"."$$rn" <= (CASE WHEN ? >= ? THEN ? ELSE "$$panic"(?) END))"#,
    );
    assert_eq!(
        compiled.statements[1].params,
        vec![int_param(1), int_param(0), int_param(1), limit_msg("take")]
    );
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$orders2".* FROM "orders" AS "$$orders2" WHERE ("$$orders2"."user_id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"()))"#,
    );
    assert!(compiled.statements[2].params.is_empty());
    assert_eq!(
        compiled.result,
        ResultShape::List {
            element: Box::new(ResultShape::Record(vec![(
                "o".to_string(),
                ResultShape::List {
                    element: Box::new(ResultShape::List {
                        element: Box::new(ResultShape::Record(vec![(
                            "total".to_string(),
                            ResultShape::Column("total".to_string()),
                        )])),
                        child: ChildRef {
                            stmt: 2,
                            parent: Some(ParentRef {
                                stmt: 1,
                                parent_key: "id".to_string(),
                                child_key: "user_id".to_string(),
                                negated: false,
                            }),
                            cardinality: Cardinality::Many,
                            tag: None,
                        },
                    }),
                    child: ChildRef {
                        stmt: 1,
                        parent: Some(ParentRef {
                            stmt: 0,
                            parent_key: "id".to_string(),
                            child_key: "manager_id".to_string(),
                            negated: false,
                        }),
                        cardinality: Cardinality::Many,
                        tag: None,
                    },
                },
            )])),
            child: ChildRef {
                stmt: 0,
                parent: None,
                cardinality: Cardinality::Many,
                tag: None,
            },
        }
    );
}

#[test]
fn nested_collection_child() {
    use crate::codegen::model::{Cardinality, ChildRef, ParentRef, ResultShape};
    let compiled = compile("users { o = subordinates.orders }");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$users1".* FROM "users" AS "$$users1" WHERE ("$$users1"."manager_id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"()))"#,
    );
    assert!(compiled.statements[1].params.is_empty());
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$orders2".* FROM "orders" AS "$$orders2" WHERE ("$$orders2"."user_id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"()))"#,
    );
    assert!(compiled.statements[2].params.is_empty());
    assert_eq!(
        compiled.result,
        ResultShape::List {
            element: Box::new(ResultShape::Record(vec![(
                "o".to_string(),
                ResultShape::List {
                    element: Box::new(ResultShape::List {
                        element: Box::new(ResultShape::Record(vec![(
                            "total".to_string(),
                            ResultShape::Column("total".to_string()),
                        )])),
                        child: ChildRef {
                            stmt: 2,
                            parent: Some(ParentRef {
                                stmt: 1,
                                parent_key: "id".to_string(),
                                child_key: "user_id".to_string(),
                                negated: false,
                            }),
                            cardinality: Cardinality::Many,
                            tag: None,
                        },
                    }),
                    child: ChildRef {
                        stmt: 1,
                        parent: Some(ParentRef {
                            stmt: 0,
                            parent_key: "id".to_string(),
                            child_key: "manager_id".to_string(),
                            negated: false,
                        }),
                        cardinality: Cardinality::Many,
                        tag: None,
                    },
                },
            )])),
            child: ChildRef {
                stmt: 0,
                parent: None,
                cardinality: Cardinality::Many,
                tag: None,
            },
        }
    );
}

#[test]
fn nested_child_first_collapse() {
    use crate::codegen::model::{Cardinality, ChildRef, ParentRef, ResultShape};
    let compiled = compile("users { o = subordinates.orders.first() }");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$subq3".* FROM (SELECT "$$users1".*, "ROW_NUMBER"() OVER (PARTITION BY "$$users1"."manager_id") AS "$$rn" FROM "users" AS "$$users1" WHERE ("$$users1"."manager_id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"()))) AS "$$subq3" WHERE ("$$subq3"."$$rn" <= ?)"#,
    );
    assert_eq!(compiled.statements[1].params, vec![int_param(1)]);
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$orders2".* FROM "orders" AS "$$orders2" WHERE ("$$orders2"."user_id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"()))"#,
    );
    assert!(compiled.statements[2].params.is_empty());
    assert_eq!(
        compiled.result,
        ResultShape::List {
            element: Box::new(ResultShape::Record(vec![(
                "o".to_string(),
                ResultShape::Single {
                    element: Box::new(ResultShape::List {
                        element: Box::new(ResultShape::Record(vec![(
                            "total".to_string(),
                            ResultShape::Column("total".to_string()),
                        )])),
                        child: ChildRef {
                            stmt: 2,
                            parent: Some(ParentRef {
                                stmt: 1,
                                parent_key: "id".to_string(),
                                child_key: "user_id".to_string(),
                                negated: false,
                            }),
                            cardinality: Cardinality::Many,
                            tag: None,
                        },
                    }),
                    child: ChildRef {
                        stmt: 1,
                        parent: Some(ParentRef {
                            stmt: 0,
                            parent_key: "id".to_string(),
                            child_key: "manager_id".to_string(),
                            negated: false,
                        }),
                        cardinality: Cardinality::One,
                        tag: None,
                    },
                },
            )])),
            child: ChildRef {
                stmt: 0,
                parent: None,
                cardinality: Cardinality::Many,
                tag: None,
            },
        }
    );
}

#[test]
fn nested_child_nth_collapse() {
    let compiled = compile("users { o = subordinates.orders.nth(0) }");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$subq3".* FROM (SELECT "$$users1".*, "ROW_NUMBER"() OVER (PARTITION BY "$$users1"."manager_id") AS "$$rn" FROM "users" AS "$$users1" WHERE ("$$users1"."manager_id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"()))) AS "$$subq3" WHERE ("$$subq3"."$$rn" = ?)"#,
    );
    assert_eq!(compiled.statements[1].params, vec![int_param(1)]);
}

#[test]
fn nested_flatten_carries_outer_key() {
    use crate::codegen::model::{Cardinality, ChildRef, ParentRef, ResultShape};
    let compiled = compile("users { o = subordinates.orders.flatten() }");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$subq3".* FROM (SELECT "$$orders2".*, (SELECT "$$users1"."manager_id" FROM "users" AS "$$users1" WHERE ("$$users1"."id" = "$$orders2"."user_id")) AS "manager_id" FROM "orders" AS "$$orders2" WHERE ("$$orders2"."user_id" IN (SELECT "$$users1"."id" FROM "users" AS "$$users1" WHERE ("$$users1"."manager_id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"()))))) AS "$$subq3""#,
    );
    assert!(compiled.statements[1].params.is_empty());
    assert_eq!(
        compiled.result,
        ResultShape::List {
            element: Box::new(ResultShape::Record(vec![(
                "o".to_string(),
                ResultShape::List {
                    element: Box::new(ResultShape::Record(vec![(
                        "total".to_string(),
                        ResultShape::Column("total".to_string()),
                    )])),
                    child: ChildRef {
                        stmt: 1,
                        parent: Some(ParentRef {
                            stmt: 0,
                            parent_key: "id".to_string(),
                            child_key: "manager_id".to_string(),
                            negated: false,
                        }),
                        cardinality: Cardinality::Many,
                        tag: None,
                    },
                },
            )])),
            child: ChildRef {
                stmt: 0,
                parent: None,
                cardinality: Cardinality::Many,
                tag: None,
            },
        }
    );
}

#[test]
fn nested_flatten_take_windows_outer_key() {
    let compiled = compile("users { o = subordinates.orders.flatten().take(1) }");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$subq4".* FROM (SELECT "$$subq3".*, "ROW_NUMBER"() OVER (PARTITION BY "$$subq3"."manager_id") AS "$$rn" FROM (SELECT "$$orders2".*, (SELECT "$$users1"."manager_id" FROM "users" AS "$$users1" WHERE ("$$users1"."id" = "$$orders2"."user_id")) AS "manager_id" FROM "orders" AS "$$orders2" WHERE ("$$orders2"."user_id" IN (SELECT "$$users1"."id" FROM "users" AS "$$users1" WHERE ("$$users1"."manager_id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"()))))) AS "$$subq3") AS "$$subq4" WHERE ("$$subq4"."$$rn" <= (CASE WHEN ? >= ? THEN ? ELSE "$$panic"(?) END))"#,
    );
    assert_eq!(
        compiled.statements[1].params,
        vec![int_param(1), int_param(0), int_param(1), limit_msg("take")]
    );
}

#[test]
fn stacked_per_parent_take_folds() {
    use crate::codegen::model::{Cardinality, ChildRef, ParentRef, ResultShape};
    let compiled = compile("users { o = orders.take(1).take(2) }");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$subq2".* FROM (SELECT "$$orders1".*, "ROW_NUMBER"() OVER (PARTITION BY "$$orders1"."user_id") AS "$$rn" FROM "orders" AS "$$orders1" WHERE ("$$orders1"."user_id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"()))) AS "$$subq2" WHERE ("$$subq2"."$$rn" <= (CASE WHEN ? >= ? THEN ? ELSE "$$panic"(?) END)) AND ("$$subq2"."$$rn" <= (CASE WHEN ? >= ? THEN ? ELSE "$$panic"(?) END))"#,
    );
    assert_eq!(
        compiled.statements[1].params,
        vec![
            int_param(1),
            int_param(0),
            int_param(1),
            limit_msg("take"),
            int_param(2),
            int_param(0),
            int_param(2),
            limit_msg("take"),
        ]
    );
    assert_eq!(
        compiled.result,
        ResultShape::List {
            element: Box::new(ResultShape::Record(vec![(
                "o".to_string(),
                ResultShape::List {
                    element: Box::new(ResultShape::Record(vec![(
                        "total".to_string(),
                        ResultShape::Column("total".to_string()),
                    )])),
                    child: ChildRef {
                        stmt: 1,
                        parent: Some(ParentRef {
                            stmt: 0,
                            parent_key: "id".to_string(),
                            child_key: "user_id".to_string(),
                            negated: false,
                        }),
                        cardinality: Cardinality::Many,
                        tag: None,
                    },
                },
            )])),
            child: ChildRef {
                stmt: 0,
                parent: None,
                cardinality: Cardinality::Many,
                tag: None,
            },
        }
    );
}

#[test]
fn stacked_per_parent_skip_take_folds() {
    use crate::codegen::model::{Cardinality, ChildRef, ParentRef, ResultShape};
    let compiled = compile("users { o = orders.skip(1).take(1) }");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$subq2".* FROM (SELECT "$$orders1".*, "ROW_NUMBER"() OVER (PARTITION BY "$$orders1"."user_id") AS "$$rn" FROM "orders" AS "$$orders1" WHERE ("$$orders1"."user_id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"()))) AS "$$subq2" WHERE ("$$subq2"."$$rn" > (CASE WHEN ? >= ? THEN ? ELSE "$$panic"(?) END)) AND ("$$subq2"."$$rn" <= "$$add"(CASE WHEN ? >= ? THEN ? ELSE "$$panic"(?) END, CASE WHEN ? >= ? THEN ? ELSE "$$panic"(?) END))"#,
    );
    assert_eq!(
        compiled.statements[1].params,
        vec![
            int_param(1),
            int_param(0),
            int_param(1),
            limit_msg("skip"),
            int_param(1),
            int_param(0),
            int_param(1),
            limit_msg("skip"),
            int_param(1),
            int_param(0),
            int_param(1),
            limit_msg("take"),
        ]
    );
    assert_eq!(
        compiled.result,
        ResultShape::List {
            element: Box::new(ResultShape::Record(vec![(
                "o".to_string(),
                ResultShape::List {
                    element: Box::new(ResultShape::Record(vec![(
                        "total".to_string(),
                        ResultShape::Column("total".to_string()),
                    )])),
                    child: ChildRef {
                        stmt: 1,
                        parent: Some(ParentRef {
                            stmt: 0,
                            parent_key: "id".to_string(),
                            child_key: "user_id".to_string(),
                            negated: false,
                        }),
                        cardinality: Cardinality::Many,
                        tag: None,
                    },
                },
            )])),
            child: ChildRef {
                stmt: 0,
                parent: None,
                cardinality: Cardinality::Many,
                tag: None,
            },
        }
    );
}

#[test]
fn stacked_sort_take_hides_inner_rn() {
    let compiled = compile("users { o = orders.skip(1).sort(total).take(1) }");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$subq2".* FROM (SELECT "$$orders1".*, "ROW_NUMBER"() OVER (PARTITION BY "$$orders1"."user_id") AS "$$rn" FROM "orders" AS "$$orders1" WHERE ("$$orders1"."user_id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"()))) AS "$$subq2" WHERE ("$$subq2"."$$rn" > (CASE WHEN ? >= ? THEN ? ELSE "$$panic"(?) END)) AND ("$$subq2"."$$rn" <= "$$add"(CASE WHEN ? >= ? THEN ? ELSE "$$panic"(?) END, CASE WHEN ? >= ? THEN ? ELSE "$$panic"(?) END)) ORDER BY "$$subq2"."total" ASC"#,
    );
    assert_eq!(
        compiled.statements[1].params,
        vec![
            int_param(1),
            int_param(0),
            int_param(1),
            limit_msg("skip"),
            int_param(1),
            int_param(0),
            int_param(1),
            limit_msg("skip"),
            int_param(1),
            int_param(0),
            int_param(1),
            limit_msg("take"),
        ]
    );
}

#[test]
fn stacked_cross_take_folds() {
    use crate::codegen::model::{Cardinality, ChildRef, ParentRef, ResultShape};
    let compiled = compile("users { x = configs.filter(owner == email).take(2).take(1) }");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$subq4".* FROM (SELECT "$$subq3".*, "ROW_NUMBER"() OVER (PARTITION BY "$$subq3"."$$pkey") AS "$$rn" FROM (SELECT "$$configs1".*, (SELECT "$$users2"."address" FROM "users" AS "$$users2" WHERE ("$$users2"."address" = "$$configs1"."owner")) AS "$$pkey" FROM "configs" AS "$$configs1") AS "$$subq3") AS "$$subq4" WHERE ("$$subq4"."$$rn" <= (CASE WHEN ? >= ? THEN ? ELSE "$$panic"(?) END)) AND ("$$subq4"."$$rn" <= (CASE WHEN ? >= ? THEN ? ELSE "$$panic"(?) END))"#,
    );
    assert_eq!(
        compiled.statements[1].params,
        vec![
            int_param(2),
            int_param(0),
            int_param(2),
            limit_msg("take"),
            int_param(1),
            int_param(0),
            int_param(1),
            limit_msg("take"),
        ]
    );
    assert_eq!(
        compiled.result,
        ResultShape::List {
            element: Box::new(ResultShape::Record(vec![(
                "x".to_string(),
                ResultShape::List {
                    element: Box::new(ResultShape::Record(vec![
                        ("name".to_string(), ResultShape::Column("name".to_string()),),
                        (
                            "owner".to_string(),
                            ResultShape::Column("owner".to_string()),
                        ),
                    ])),
                    child: ChildRef {
                        stmt: 1,
                        parent: Some(ParentRef {
                            stmt: 0,
                            parent_key: "address".to_string(),
                            child_key: "$$pkey".to_string(),
                            negated: false,
                        }),
                        cardinality: Cardinality::Many,
                        tag: None,
                    },
                },
            )])),
            child: ChildRef {
                stmt: 0,
                parent: None,
                cardinality: Cardinality::Many,
                tag: None,
            },
        }
    );
}

#[test]
fn stacked_cross_skip_take_folds() {
    let compiled = compile("users { x = configs.filter(owner == email).skip(1).take(1) }");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$subq4".* FROM (SELECT "$$subq3".*, "ROW_NUMBER"() OVER (PARTITION BY "$$subq3"."$$pkey") AS "$$rn" FROM (SELECT "$$configs1".*, (SELECT "$$users2"."address" FROM "users" AS "$$users2" WHERE ("$$users2"."address" = "$$configs1"."owner")) AS "$$pkey" FROM "configs" AS "$$configs1") AS "$$subq3") AS "$$subq4" WHERE ("$$subq4"."$$rn" > (CASE WHEN ? >= ? THEN ? ELSE "$$panic"(?) END)) AND ("$$subq4"."$$rn" <= "$$add"(CASE WHEN ? >= ? THEN ? ELSE "$$panic"(?) END, CASE WHEN ? >= ? THEN ? ELSE "$$panic"(?) END))"#,
    );
    assert_eq!(
        compiled.statements[1].params,
        vec![
            int_param(1),
            int_param(0),
            int_param(1),
            limit_msg("skip"),
            int_param(1),
            int_param(0),
            int_param(1),
            limit_msg("skip"),
            int_param(1),
            int_param(0),
            int_param(1),
            limit_msg("take"),
        ]
    );
}

#[test]
fn stacked_nested_take_folds() {
    let compiled = compile("users { o = subordinates.orders.skip(1).take(1) }");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$subq3".* FROM (SELECT "$$users1".*, "ROW_NUMBER"() OVER (PARTITION BY "$$users1"."manager_id") AS "$$rn" FROM "users" AS "$$users1" WHERE ("$$users1"."manager_id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"()))) AS "$$subq3" WHERE ("$$subq3"."$$rn" > (CASE WHEN ? >= ? THEN ? ELSE "$$panic"(?) END)) AND ("$$subq3"."$$rn" <= "$$add"(CASE WHEN ? >= ? THEN ? ELSE "$$panic"(?) END, CASE WHEN ? >= ? THEN ? ELSE "$$panic"(?) END))"#,
    );
    assert_eq!(
        compiled.statements[1].params,
        vec![
            int_param(1),
            int_param(0),
            int_param(1),
            limit_msg("skip"),
            int_param(1),
            int_param(0),
            int_param(1),
            limit_msg("skip"),
            int_param(1),
            int_param(0),
            int_param(1),
            limit_msg("take"),
        ]
    );
}

#[test]
fn stacked_nested_take_first_folds() {
    let compiled = compile("users { o = subordinates.orders.take(1).first() }");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$subq3".* FROM (SELECT "$$users1".*, "ROW_NUMBER"() OVER (PARTITION BY "$$users1"."manager_id") AS "$$rn" FROM "users" AS "$$users1" WHERE ("$$users1"."manager_id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"()))) AS "$$subq3" WHERE ("$$subq3"."$$rn" <= (CASE WHEN ? >= ? THEN ? ELSE "$$panic"(?) END)) AND ("$$subq3"."$$rn" <= ?)"#,
    );
    assert_eq!(
        compiled.statements[1].params,
        vec![
            int_param(1),
            int_param(0),
            int_param(1),
            limit_msg("take"),
            int_param(1),
        ]
    );
}

#[test]
fn take_over_broadcast_truncates() {
    use crate::codegen::model::{Cardinality, ChildRef, ParentRef, ResultShape};
    let compiled =
        compile("users { x = configs.filter(owner == email || name == \"n1\").take(1) }");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$configs1".*, (SELECT "$$users2"."address" FROM "users" AS "$$users2" WHERE (("$$users2"."address" = "$$configs1"."owner") AND NOT ("$$configs1"."name" = ?))) AS "$$pkey", ? AS "$$ptag" FROM "configs" AS "$$configs1" UNION ALL SELECT "$$configs1".*, ? AS "$$pkey", ? AS "$$ptag" FROM "configs" AS "$$configs1" WHERE ("$$configs1"."name" = ?)"#,
    );
    assert_eq!(
        compiled.statements[1].params,
        vec![
            str_param("n1"),
            int_param(0),
            Param::Value(ParamValue::Null),
            int_param(1),
            str_param("n1"),
        ]
    );
    assert_eq!(
        compiled.result,
        ResultShape::List {
            element: Box::new(ResultShape::Record(vec![(
                "x".to_string(),
                ResultShape::List {
                    element: Box::new(ResultShape::Record(vec![
                        ("name".to_string(), ResultShape::Column("name".to_string()),),
                        (
                            "owner".to_string(),
                            ResultShape::Column("owner".to_string()),
                        ),
                    ])),
                    child: ChildRef {
                        stmt: 1,
                        parent: Some(ParentRef {
                            stmt: 0,
                            parent_key: "address".to_string(),
                            child_key: "$$pkey".to_string(),
                            negated: false,
                        }),
                        cardinality: Cardinality::Take {
                            offset: 0,
                            limit: Some(1),
                        },
                        tag: None,
                    },
                },
            )])),
            child: ChildRef {
                stmt: 0,
                parent: None,
                cardinality: Cardinality::Many,
                tag: None,
            },
        }
    );
}

#[test]
fn stacked_take_over_broadcast_folds() {
    use crate::codegen::model::{Cardinality, ChildRef, ParentRef, ResultShape};
    let compiled =
        compile("users { x = configs.filter(owner == email || name == \"n1\").skip(1).take(1) }");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$configs1".*, (SELECT "$$users2"."address" FROM "users" AS "$$users2" WHERE (("$$users2"."address" = "$$configs1"."owner") AND NOT ("$$configs1"."name" = ?))) AS "$$pkey", ? AS "$$ptag" FROM "configs" AS "$$configs1" UNION ALL SELECT "$$configs1".*, ? AS "$$pkey", ? AS "$$ptag" FROM "configs" AS "$$configs1" WHERE ("$$configs1"."name" = ?)"#,
    );
    assert_eq!(
        compiled.result,
        ResultShape::List {
            element: Box::new(ResultShape::Record(vec![(
                "x".to_string(),
                ResultShape::List {
                    element: Box::new(ResultShape::Record(vec![
                        ("name".to_string(), ResultShape::Column("name".to_string()),),
                        (
                            "owner".to_string(),
                            ResultShape::Column("owner".to_string()),
                        ),
                    ])),
                    child: ChildRef {
                        stmt: 1,
                        parent: Some(ParentRef {
                            stmt: 0,
                            parent_key: "address".to_string(),
                            child_key: "$$pkey".to_string(),
                            negated: false,
                        }),
                        cardinality: Cardinality::Take {
                            offset: 1,
                            limit: Some(1),
                        },
                        tag: None,
                    },
                },
            )])),
            child: ChildRef {
                stmt: 0,
                parent: None,
                cardinality: Cardinality::Many,
                tag: None,
            },
        }
    );
}

#[test]
fn cross_root_in_child_tail() {
    use crate::codegen::model::{Cardinality, ChildRef, ParentRef, ResultShape};
    let compiled = compile("users { o = orders { s = configs.len() } }");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT (SELECT COUNT(*) AS "$$val1" FROM "configs" AS "$$configs0") AS "s", "$$orders1"."user_id" FROM "orders" AS "$$orders1" WHERE ("$$orders1"."user_id" IN (SELECT "$$split1"."$$key" FROM "$$split1" AS "$$split1"))"#,
    );
    assert_eq!(
        compiled.result,
        ResultShape::List {
            element: Box::new(ResultShape::Record(vec![(
                "o".to_string(),
                ResultShape::List {
                    element: Box::new(ResultShape::Record(vec![(
                        "s".to_string(),
                        ResultShape::Column("s".to_string()),
                    )])),
                    child: ChildRef {
                        stmt: 2,
                        parent: Some(ParentRef {
                            stmt: 0,
                            parent_key: "$$key".to_string(),
                            child_key: "user_id".to_string(),
                            negated: false,
                        }),
                        cardinality: Cardinality::Many,
                        tag: None,
                    },
                },
            )])),
            child: ChildRef {
                stmt: 1,
                parent: None,
                cardinality: Cardinality::Many,
                tag: None,
            },
        }
    );
}

#[test]
fn nested_collection_root_in_child_tail() {
    use crate::codegen::model::{Cardinality, ChildRef, ParentRef, ResultShape};
    let compiled = compile("users { o = orders { s = subordinates.len() } }");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT (SELECT COUNT(*) AS "$$val1" FROM "users" AS "$$users0" WHERE ("$$users0"."manager_id" = "$$orders1"."user_id")) AS "s", "$$orders1"."user_id" FROM "orders" AS "$$orders1" WHERE ("$$orders1"."user_id" IN (SELECT "$$split1"."$$key" FROM "$$split1" AS "$$split1"))"#,
    );
    assert_eq!(
        compiled.result,
        ResultShape::List {
            element: Box::new(ResultShape::Record(vec![(
                "o".to_string(),
                ResultShape::List {
                    element: Box::new(ResultShape::Record(vec![(
                        "s".to_string(),
                        ResultShape::Column("s".to_string()),
                    )])),
                    child: ChildRef {
                        stmt: 2,
                        parent: Some(ParentRef {
                            stmt: 0,
                            parent_key: "$$key".to_string(),
                            child_key: "user_id".to_string(),
                            negated: false,
                        }),
                        cardinality: Cardinality::Many,
                        tag: None,
                    },
                },
            )])),
            child: ChildRef {
                stmt: 1,
                parent: None,
                cardinality: Cardinality::Many,
                tag: None,
            },
        }
    );
}

#[test]
fn projected_records_nav_relowers() {
    let compiled = compile("users { m = manager } { x = m.name }");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"SELECT (SELECT "$$users2"."name" FROM "users" AS "$$users2" WHERE ("$$users2"."id" = "$$subq3"."manager_id")) AS "x" FROM (SELECT "$$users0"."manager_id" FROM "users" AS "$$users0") AS "$$subq3""#,
    );
}

#[test]
fn unwrap_record_rows_falls_back() {
    let compiled = compile("users { x = mentor.unwrap_or(manager).name }");
    assert_eq!(compiled.statements.len(), 1);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"SELECT (SELECT "$$subq6"."name" FROM (SELECT "$$subq2".* FROM (SELECT "$$users1".* FROM "users" AS "$$users1" WHERE ("$$users1"."id" = "$$users0"."mentor_id")) AS "$$subq2" UNION ALL SELECT "$$subq4".* FROM (SELECT "$$users3".* FROM "users" AS "$$users3" WHERE ("$$users3"."id" = "$$users0"."manager_id")) AS "$$subq4" WHERE NOT EXISTS (SELECT ? AS "c0" FROM (SELECT "$$subq2".* FROM (SELECT "$$users1".* FROM "users" AS "$$users1" WHERE ("$$users1"."id" = "$$users0"."mentor_id")) AS "$$subq2") AS "$$subq5")) AS "$$subq6") AS "x" FROM "users" AS "$$users0""#,
    );
    assert_eq!(compiled.statements[0].params, vec![int_param(1)]);
}

#[test]
fn unwrap_record_rows_keep_siblings() {
    let compiled = compile("users { x = mentor.unwrap_or(manager).name, y = name }");
    assert_eq!(compiled.statements.len(), 1);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"SELECT (SELECT "$$subq6"."name" FROM (SELECT "$$subq2".* FROM (SELECT "$$users1".* FROM "users" AS "$$users1" WHERE ("$$users1"."id" = "$$users0"."mentor_id")) AS "$$subq2" UNION ALL SELECT "$$subq4".* FROM (SELECT "$$users3".* FROM "users" AS "$$users3" WHERE ("$$users3"."id" = "$$users0"."manager_id")) AS "$$subq4" WHERE NOT EXISTS (SELECT ? AS "c0" FROM (SELECT "$$subq2".* FROM (SELECT "$$users1".* FROM "users" AS "$$users1" WHERE ("$$users1"."id" = "$$users0"."mentor_id")) AS "$$subq2") AS "$$subq5")) AS "$$subq6") AS "x", "$$users0"."name" AS "y" FROM "users" AS "$$users0""#,
    );
}

#[test]
fn projected_nonscalar_field_routed() {
    let compiled = compile("users { o = orders }.o");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"SELECT "$$users0"."id" FROM "users" AS "$$users0""#,
    );
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$orders1".* FROM "orders" AS "$$orders1" WHERE ("$$orders1"."user_id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"()))"#,
    );
    assert!(compiled.statements.iter().all(|s| s.params.is_empty()));
}

#[test]
fn cross_projection_outer_consts() {
    use crate::codegen::model::{Cardinality, ChildRef, ParentRef, ResultShape};
    let compiled = compile("configs { u = users { n = owner } }");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"CREATE TEMP TABLE "$$split1" AS SELECT "$$configs0".*, "ROW_NUMBER"() OVER () AS "$$key" FROM "configs" AS "$$configs0""#,
    );
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$split1"."$$key" FROM "$$split1" AS "$$split1""#,
    );
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$split1"."owner" AS "n", "$$split1"."$$key" AS "$$pkey" FROM "$$split1" AS "$$split1", "users" AS "$$users2""#,
    );
    assert!(compiled.statements.iter().all(|s| s.params.is_empty()));
    assert_eq!(
        compiled.result,
        ResultShape::List {
            element: Box::new(ResultShape::Record(vec![(
                "u".to_string(),
                ResultShape::List {
                    element: Box::new(ResultShape::Record(vec![(
                        "n".to_string(),
                        ResultShape::Column("n".to_string())
                    )])),
                    child: ChildRef {
                        stmt: 2,
                        parent: Some(ParentRef {
                            stmt: 0,
                            parent_key: "$$key".to_string(),
                            child_key: "$$pkey".to_string(),
                            negated: false,
                        }),
                        cardinality: Cardinality::Many,
                        tag: None,
                    },
                },
            )])),
            child: ChildRef {
                stmt: 1,
                parent: None,
                cardinality: Cardinality::Many,
                tag: None,
            },
        }
    );
}

#[test]
fn cross_projection_pipeline_take_consts() {
    let compiled = compile("configs { u = users.take(1) { n = owner } }");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"CREATE TEMP TABLE "$$split3" AS SELECT "$$configs0".*, "ROW_NUMBER"() OVER () AS "$$key" FROM "configs" AS "$$configs0""#,
    );
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$split3"."owner" AS "n", "$$split3"."$$key" AS "$$pkey" FROM "$$split3" AS "$$split3", (SELECT "$$users1"."name", "$$users1"."address", "$$users1"."age", "$$users1"."score", "$$users1"."balance", "$$users1"."active", "$$users1"."nickname", "$$users1"."lat", "$$users1"."lng", "$$users1"."t0", "$$users1"."t1", "$$users1"."t2", "$$users1"."created", "$$users1"."ttl", "$$users1"."id", "$$users1"."manager_id", "$$users1"."mentor_id" FROM "users" AS "$$users1" LIMIT (CASE WHEN ? >= ? THEN ? ELSE "$$panic"(?) END)) AS "$$subq2""#,
    );
    assert_eq!(
        compiled.statements[2].params,
        vec![
            int_param(1),
            int_param(0),
            int_param(1),
            Param::Value(ParamValue::String(
                "take requires a non-negative integer".to_string()
            )),
        ]
    );
}

#[test]
fn cross_projection_pipeline_filter_consts() {
    let compiled = compile("configs { u = users.filter(age > 1) { n = owner } }");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$split3"."owner" AS "n", "$$split3"."$$key" AS "$$pkey" FROM "$$split3" AS "$$split3", (SELECT "$$users1"."name", "$$users1"."address", "$$users1"."age", "$$users1"."score", "$$users1"."balance", "$$users1"."active", "$$users1"."nickname", "$$users1"."lat", "$$users1"."lng", "$$users1"."t0", "$$users1"."t1", "$$users1"."t2", "$$users1"."created", "$$users1"."ttl", "$$users1"."id", "$$users1"."manager_id", "$$users1"."mentor_id" FROM "users" AS "$$users1" WHERE ("$$users1"."age" > ?)) AS "$$subq2""#,
    );
    assert_eq!(compiled.statements[2].params, vec![int_param(1)]);
}

#[test]
fn cross_projection_list_inner() {
    use crate::codegen::model::{Cardinality, ChildRef, ParentRef, ResultShape};
    let compiled = compile("users { x = configs { s = scores } }");
    assert_eq!(compiled.statements.len(), 4);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"CREATE TEMP TABLE "$$split3" AS SELECT "$$users0".*, "ROW_NUMBER"() OVER () AS "$$key" FROM "users" AS "$$users0""#,
    );
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$configs4"."name", "$$configs4"."owner", "$$split3"."$$key" AS "$$pkey", "$$split3"."id" FROM "$$split3" AS "$$split3", "configs" AS "$$configs4""#,
    );
    assert_sql_eq(
        &compiled.statements[3].sql,
        r#"SELECT "$$user_scores5".* FROM "user_scores" AS "$$user_scores5" WHERE ("$$user_scores5"."user_key" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"()))"#,
    );
    assert!(compiled.statements.iter().all(|s| s.params.is_empty()));
    assert_eq!(
        compiled.result,
        ResultShape::List {
            element: Box::new(ResultShape::Record(vec![(
                "x".to_string(),
                ResultShape::List {
                    element: Box::new(ResultShape::Record(vec![(
                        "s".to_string(),
                        ResultShape::List {
                            element: Box::new(ResultShape::Column("score_value".to_string())),
                            child: ChildRef {
                                stmt: 3,
                                parent: Some(ParentRef {
                                    stmt: 2,
                                    parent_key: "id".to_string(),
                                    child_key: "user_key".to_string(),
                                    negated: false,
                                }),
                                cardinality: Cardinality::Many,
                                tag: None,
                            },
                        },
                    )])),
                    child: ChildRef {
                        stmt: 2,
                        parent: Some(ParentRef {
                            stmt: 0,
                            parent_key: "$$key".to_string(),
                            child_key: "$$pkey".to_string(),
                            negated: false,
                        }),
                        cardinality: Cardinality::Many,
                        tag: None,
                    },
                },
            )])),
            child: ChildRef {
                stmt: 1,
                parent: None,
                cardinality: Cardinality::Many,
                tag: None,
            },
        }
    );
}

#[test]
fn cross_projection_pipeline_list_inner() {
    let compiled = compile("users { x = configs.take(1) { s = scores } }");
    assert_eq!(compiled.statements.len(), 4);
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$subq4"."name", "$$subq4"."owner", "$$split5"."$$key" AS "$$pkey", "$$split5"."id" FROM "$$split5" AS "$$split5", (SELECT "$$configs3"."name", "$$configs3"."owner" FROM "configs" AS "$$configs3" LIMIT (CASE WHEN ? >= ? THEN ? ELSE "$$panic"(?) END)) AS "$$subq4""#,
    );
    assert_eq!(
        compiled.statements[2].params,
        vec![
            int_param(1),
            int_param(0),
            int_param(1),
            Param::Value(ParamValue::String(
                "take requires a non-negative integer".to_string()
            )),
        ]
    );
}

#[test]
fn cross_projection_corr_filter_list_inner() {
    use crate::codegen::model::{Cardinality, ChildRef, ParentRef, ResultShape};
    let compiled = compile("users { x = configs.filter(owner == email) { s = scores } }");
    assert_eq!(compiled.statements.len(), 4);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"CREATE TEMP TABLE "$$split3" AS SELECT "$$users0".*, "ROW_NUMBER"() OVER () AS "$$key" FROM "users" AS "$$users0""#,
    );
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$configs4"."name", "$$configs4"."owner", "$$split3"."$$key" AS "$$pkey", "$$split3"."id" FROM "$$split3" AS "$$split3", "configs" AS "$$configs4" WHERE ("$$configs4"."owner" = "$$split3"."address")"#,
    );
    assert_sql_eq(
        &compiled.statements[3].sql,
        r#"SELECT "$$user_scores5".* FROM "user_scores" AS "$$user_scores5" WHERE ("$$user_scores5"."user_key" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"()))"#,
    );
    assert!(compiled.statements.iter().all(|s| s.params.is_empty()));
    assert_eq!(
        compiled.result,
        ResultShape::List {
            element: Box::new(ResultShape::Record(vec![(
                "x".to_string(),
                ResultShape::List {
                    element: Box::new(ResultShape::Record(vec![(
                        "s".to_string(),
                        ResultShape::List {
                            element: Box::new(ResultShape::Column("score_value".to_string())),
                            child: ChildRef {
                                stmt: 3,
                                parent: Some(ParentRef {
                                    stmt: 2,
                                    parent_key: "id".to_string(),
                                    child_key: "user_key".to_string(),
                                    negated: false,
                                }),
                                cardinality: Cardinality::Many,
                                tag: None,
                            },
                        },
                    )])),
                    child: ChildRef {
                        stmt: 2,
                        parent: Some(ParentRef {
                            stmt: 0,
                            parent_key: "$$key".to_string(),
                            child_key: "$$pkey".to_string(),
                            negated: false,
                        }),
                        cardinality: Cardinality::Many,
                        tag: None,
                    },
                },
            )])),
            child: ChildRef {
                stmt: 1,
                parent: None,
                cardinality: Cardinality::Many,
                tag: None,
            },
        }
    );
}

#[test]
fn cross_projection_corr_filter_outer_scalar() {
    use crate::codegen::model::{Cardinality, ChildRef, ParentRef, ResultShape};
    let compiled = compile("users { x = configs.filter(owner == email) { s = age } }");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$split2"."age" AS "s", "$$split2"."$$key" AS "$$pkey" FROM "$$split2" AS "$$split2", "configs" AS "$$configs3" WHERE ("$$configs3"."owner" = "$$split2"."address")"#,
    );
    assert!(compiled.statements.iter().all(|s| s.params.is_empty()));
    assert_eq!(
        compiled.result,
        ResultShape::List {
            element: Box::new(ResultShape::Record(vec![(
                "x".to_string(),
                ResultShape::List {
                    element: Box::new(ResultShape::Record(vec![(
                        "s".to_string(),
                        ResultShape::Column("s".to_string()),
                    )])),
                    child: ChildRef {
                        stmt: 2,
                        parent: Some(ParentRef {
                            stmt: 0,
                            parent_key: "$$key".to_string(),
                            child_key: "$$pkey".to_string(),
                            negated: false,
                        }),
                        cardinality: Cardinality::Many,
                        tag: None,
                    },
                },
            )])),
            child: ChildRef {
                stmt: 1,
                parent: None,
                cardinality: Cardinality::Many,
                tag: None,
            },
        }
    );
}

#[test]
fn cross_projection_corr_filter_nav_inner() {
    use crate::codegen::model::{Cardinality, ChildRef, ParentRef, ResultShape};
    let compiled = compile("users { x = configs.filter(owner == email) { s = manager.orders } }");
    assert_eq!(compiled.statements.len(), 4);
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$configs4"."name", "$$configs4"."owner", "$$split3"."$$key" AS "$$pkey", "$$split3"."manager_id" FROM "$$split3" AS "$$split3", "configs" AS "$$configs4" WHERE ("$$configs4"."owner" = "$$split3"."address")"#,
    );
    assert_sql_eq(
        &compiled.statements[3].sql,
        r#"SELECT "$$orders5".* FROM "orders" AS "$$orders5" WHERE ("$$orders5"."user_id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"()))"#,
    );
    assert!(compiled.statements.iter().all(|s| s.params.is_empty()));
    assert_eq!(
        compiled.result,
        ResultShape::List {
            element: Box::new(ResultShape::Record(vec![(
                "x".to_string(),
                ResultShape::List {
                    element: Box::new(ResultShape::Record(vec![(
                        "s".to_string(),
                        ResultShape::List {
                            element: Box::new(ResultShape::Record(vec![(
                                "total".to_string(),
                                ResultShape::Column("total".to_string()),
                            )])),
                            child: ChildRef {
                                stmt: 3,
                                parent: Some(ParentRef {
                                    stmt: 2,
                                    parent_key: "manager_id".to_string(),
                                    child_key: "user_id".to_string(),
                                    negated: false,
                                }),
                                cardinality: Cardinality::Many,
                                tag: None,
                            },
                        },
                    )])),
                    child: ChildRef {
                        stmt: 2,
                        parent: Some(ParentRef {
                            stmt: 0,
                            parent_key: "$$key".to_string(),
                            child_key: "$$pkey".to_string(),
                            negated: false,
                        }),
                        cardinality: Cardinality::Many,
                        tag: None,
                    },
                },
            )])),
            child: ChildRef {
                stmt: 1,
                parent: None,
                cardinality: Cardinality::Many,
                tag: None,
            },
        }
    );
}

#[test]
fn cross_projection_nested_correlated() {
    let compiled = compile("configs { u = users.filter(email == owner) }");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$users0".*, (SELECT "$$configs1"."owner" FROM "configs" AS "$$configs1" WHERE ("$$configs1"."owner" = "$$users0"."address")) AS "$$pkey" FROM "users" AS "$$users0""#,
    );
    assert!(compiled.statements.iter().all(|s| s.params.is_empty()));
}

#[test]
fn lit_deep_nesting_result() {
    let compiled = compile("[[[1]]]");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"SELECT "lit0"."c0" FROM "$$lit_1_I"('lit0') AS "lit0""#,
    );
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "lit1".* FROM "$$lit_2_II"('lit1') AS "lit1" WHERE ("lit1"."c0" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"()))"#,
    );
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "lit2".* FROM "$$lit_2_II"('lit2') AS "lit2" WHERE ("lit2"."c0" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"()))"#,
    );
    let mut expected = HashMap::new();
    expected.insert("lit0".to_string(), vec![vec![ParamValue::Int(0)]]);
    expected.insert(
        "lit1".to_string(),
        vec![vec![ParamValue::Int(0), ParamValue::Int(0)]],
    );
    expected.insert(
        "lit2".to_string(),
        vec![vec![ParamValue::Int(0), ParamValue::Int(1)]],
    );
    assert_eq!(compiled.literals, expected);
}

#[test]
fn lit_deep_double_flatten() {
    let (sql, params) = sql("[[[1]]].flatten().flatten()");
    assert_sql_eq(
        &sql,
        r#"SELECT "lit2"."c1" AS "c0" FROM "$$lit_2_II"('lit2') AS "lit2" WHERE ("lit2"."c0" IN (SELECT "lit1"."c1" FROM "$$lit_2_II"('lit1') AS "lit1" WHERE ("lit1"."c0" IN (SELECT "lit0"."c0" FROM "$$lit_1_I"('lit0') AS "lit0"))))"#,
    );
    assert!(params.is_empty());
}

#[test]
fn lit_record_named_fields() {
    use crate::codegen::model::{Cardinality, ChildRef, ResultShape};
    let compiled = compile("[{ name = \"Bob\", age = 40 }]");
    assert_eq!(compiled.statements.len(), 1);
    assert_eq!(
        compiled.result,
        ResultShape::List {
            element: Box::new(ResultShape::Record(vec![
                ("name".to_string(), ResultShape::Column("c0".to_string())),
                ("age".to_string(), ResultShape::Column("c1".to_string())),
            ])),
            child: ChildRef {
                stmt: 0,
                parent: None,
                cardinality: Cardinality::Many,
                tag: None,
            },
        }
    );
    let mut expected = HashMap::new();
    expected.insert(
        "lit0".to_string(),
        vec![vec![
            ParamValue::String("Bob".to_string()),
            ParamValue::Int(40),
        ]],
    );
    assert_eq!(compiled.literals, expected);
}

#[test]
fn lit_nested_item() {
    let compiled = compile("users { x = [[1], [2]] }");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"SELECT "$$users0".* FROM "users" AS "$$users0""#,
    );
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "lit1".* FROM "$$lit_1_I"('lit1') AS "lit1""#,
    );
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "lit2".* FROM "$$lit_2_II"('lit2') AS "lit2" WHERE ("lit2"."c0" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"()))"#,
    );
}

#[test]
fn lit_query_valued_elements() {
    use crate::codegen::model::{Cardinality, ChildRef, ParentRef, ResultShape};
    let compiled = compile("[[1].take(1)]");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"SELECT "lit1"."c0" FROM "$$lit_1_I"('lit1') AS "lit1""#,
    );
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "lit3".* FROM (SELECT ? AS "c0", "$$subq2"."c0" AS "c1" FROM (SELECT "lit0".* FROM "$$lit_1_I"('lit0') AS "lit0" LIMIT (CASE WHEN ? >= ? THEN ? ELSE "$$panic"(?) END)) AS "$$subq2") AS "lit3" WHERE ("lit3"."c0" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"()))"#,
    );
    assert_eq!(
        compiled.result,
        ResultShape::List {
            element: Box::new(ResultShape::List {
                element: Box::new(ResultShape::Column("c1".to_string())),
                child: ChildRef {
                    stmt: 1,
                    parent: Some(ParentRef {
                        stmt: 0,
                        parent_key: "c0".to_string(),
                        child_key: "c0".to_string(),
                        negated: false,
                    }),
                    cardinality: Cardinality::Many,
                    tag: None,
                },
            }),
            child: ChildRef {
                stmt: 1,
                parent: Some(ParentRef {
                    stmt: 0,
                    parent_key: "c0".to_string(),
                    child_key: "c0".to_string(),
                    negated: false,
                }),
                cardinality: Cardinality::Many,
                tag: None,
            },
        }
    );
}

#[test]
fn lit_query_temp_grid() {
    use crate::codegen::model::{Cardinality, ChildRef, ParentRef, ResultShape};
    let compiled = compile("users { x = [orders.take(1)] }");
    assert_eq!(compiled.statements.len(), 4);
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$split1"."$$key" FROM "$$split1" AS "$$split1""#,
    );
    assert_sql_eq(
        &compiled.statements[3].sql,
        r#"SELECT "$$split3"."$$key" AS "c0", "$$subq5"."total", "$$subq5"."user_id", "$$subq5"."id", "$$subq5"."$$rn" FROM "$$split3" AS "$$split3", (SELECT "$$orders2".*, "ROW_NUMBER"() OVER (PARTITION BY "$$orders2"."user_id") AS "$$rn" FROM "orders" AS "$$orders2") AS "$$subq5" WHERE ("$$subq5"."user_id" = "$$split3"."id") AND ("$$subq5"."$$rn" <= (CASE WHEN ? >= ? THEN ? ELSE "$$panic"(?) END))"#,
    );
    assert_eq!(
        compiled.result,
        ResultShape::List {
            element: Box::new(ResultShape::Record(vec![(
                "x".to_string(),
                ResultShape::List {
                    element: Box::new(ResultShape::List {
                        element: Box::new(ResultShape::Record(vec![(
                            "total".to_string(),
                            ResultShape::Column("total".to_string()),
                        )])),
                        child: ChildRef {
                            stmt: 3,
                            parent: Some(ParentRef {
                                stmt: 1,
                                parent_key: "$$key".to_string(),
                                child_key: "c0".to_string(),
                                negated: false,
                            }),
                            cardinality: Cardinality::Many,
                            tag: None,
                        },
                    }),
                    child: ChildRef {
                        stmt: 1,
                        parent: Some(ParentRef {
                            stmt: 0,
                            parent_key: "$$key".to_string(),
                            child_key: "c0".to_string(),
                            negated: false,
                        }),
                        cardinality: Cardinality::Many,
                        tag: None,
                    },
                },
            )])),
            child: ChildRef {
                stmt: 2,
                parent: None,
                cardinality: Cardinality::Many,
                tag: None,
            },
        }
    );
}

#[test]
fn lit_query_temp_flat() {
    let compiled = compile("users { x = [users.first()] }");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$split1"."$$key" AS "c0", "$$subq3"."name", "$$subq3"."address", "$$subq3"."age", "$$subq3"."score", "$$subq3"."balance", "$$subq3"."active", "$$subq3"."nickname", "$$subq3"."lat", "$$subq3"."lng", "$$subq3"."t0", "$$subq3"."t1", "$$subq3"."t2", "$$subq3"."created", "$$subq3"."ttl", "$$subq3"."id", "$$subq3"."manager_id", "$$subq3"."mentor_id" FROM "$$split1" AS "$$split1", (SELECT "$$users2".* FROM "users" AS "$$users2" LIMIT ?) AS "$$subq3""#,
    );
    let compiled = compile("users { x = [orders.first()] }");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$split1"."$$key" AS "c0", "$$orders2"."total", "$$orders2"."user_id", "$$orders2"."id" FROM "$$split1" AS "$$split1", "orders" AS "$$orders2" WHERE ("$$orders2"."user_id" = "$$split1"."id") UNION ALL SELECT "$$split1"."$$key" AS "c0", ? AS "total", ? AS "user_id", ? AS "id" FROM "$$split1" AS "$$split1" WHERE NOT EXISTS (SELECT ? AS "one" FROM "orders" AS "$$exists3" WHERE ("$$exists3"."user_id" = "$$split1"."id"))"#,
    );
    let compiled = compile("users { x = [(orders.first()?.total)] }");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$split1"."$$key" AS "c0", (SELECT "$$orders2"."total" FROM "orders" AS "$$orders2" WHERE ("$$orders2"."user_id" = "$$split1"."id")) AS "c1" FROM "$$split1" AS "$$split1""#,
    );
}

#[test]
fn lit_multi_list_fork() {
    use crate::codegen::model::{Cardinality, ChildRef, ParentRef, ResultShape};
    let compiled = compile("users { x = [{a = [age], b = [1]}] }");
    assert_eq!(compiled.statements.len(), 5);
    assert_sql_eq(
        &compiled.statements[3].sql,
        r#"SELECT "$$split2"."$$key" AS "c0", ? AS "c1" FROM "$$split2" AS "$$split2" WHERE ("$$split2"."c1" = ?)"#,
    );
    assert_sql_eq(
        &compiled.statements[4].sql,
        r#"SELECT "$$split2"."$$key" AS "c0", "$$split2"."age" AS "c1" FROM "$$split2" AS "$$split2" WHERE ("$$split2"."c1" = ?)"#,
    );
    assert_eq!(compiled.statements[4].params, vec![int_param(0)],);
    assert_eq!(
        compiled.result,
        ResultShape::List {
            element: Box::new(ResultShape::Record(vec![(
                "x".to_string(),
                ResultShape::List {
                    element: Box::new(ResultShape::Record(vec![
                        (
                            "a".to_string(),
                            ResultShape::List {
                                element: Box::new(ResultShape::Column("c1".to_string())),
                                child: ChildRef {
                                    stmt: 4,
                                    parent: Some(ParentRef {
                                        stmt: 1,
                                        parent_key: "$$key".to_string(),
                                        child_key: "c0".to_string(),
                                        negated: false,
                                    }),
                                    cardinality: Cardinality::Many,
                                    tag: None,
                                },
                            },
                        ),
                        (
                            "b".to_string(),
                            ResultShape::List {
                                element: Box::new(ResultShape::Column("c1".to_string())),
                                child: ChildRef {
                                    stmt: 3,
                                    parent: Some(ParentRef {
                                        stmt: 1,
                                        parent_key: "$$key".to_string(),
                                        child_key: "c0".to_string(),
                                        negated: false,
                                    }),
                                    cardinality: Cardinality::Many,
                                    tag: None,
                                },
                            },
                        ),
                    ])),
                    child: ChildRef {
                        stmt: 1,
                        parent: Some(ParentRef {
                            stmt: 0,
                            parent_key: "$$key".to_string(),
                            child_key: "c0".to_string(),
                            negated: false,
                        }),
                        cardinality: Cardinality::Many,
                        tag: None,
                    },
                },
            )])),
            child: ChildRef {
                stmt: 2,
                parent: None,
                cardinality: Cardinality::Many,
                tag: None,
            },
        }
    );
}

#[test]
fn lit_take_bound_temp_window() {
    let compiled = compile("users { x = [1, 2, 3].take(age) }");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$subq3".* FROM (SELECT "$$subq2".*, "ROW_NUMBER"() OVER (PARTITION BY "$$subq2"."c0") AS "$$rn" FROM (SELECT "$$split1"."$$key" AS "c0", ? AS "c1" FROM "$$split1" AS "$$split1" UNION ALL SELECT "$$split1"."$$key" AS "c0", ? AS "c1" FROM "$$split1" AS "$$split1" UNION ALL SELECT "$$split1"."$$key" AS "c0", ? AS "c1" FROM "$$split1" AS "$$split1") AS "$$subq2") AS "$$subq3" WHERE ("$$subq3"."$$rn" <= (CASE WHEN (SELECT "$$split1"."age" FROM "$$split1" AS "$$split1" WHERE ("$$split1"."$$key" = "$$subq3"."c0")) >= ? THEN (SELECT "$$split1"."age" FROM "$$split1" AS "$$split1" WHERE ("$$split1"."$$key" = "$$subq3"."c0")) ELSE "$$panic"(?) END))"#,
    );
}

#[test]
fn lit_take_bound_nav_temp_window() {
    let compiled = compile("users { x = [1].take(manager.age) }");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$subq4".* FROM (SELECT "$$subq2".*, "ROW_NUMBER"() OVER (PARTITION BY "$$subq2"."c0") AS "$$rn" FROM (SELECT "$$split1"."$$key" AS "c0", ? AS "c1" FROM "$$split1" AS "$$split1") AS "$$subq2") AS "$$subq4" WHERE ("$$subq4"."$$rn" <= (CASE WHEN (SELECT "$$users3"."age" FROM "users" AS "$$users3" WHERE ("$$users3"."id" = (SELECT "$$split1"."manager_id" FROM "$$split1" AS "$$split1" WHERE ("$$split1"."$$key" = "$$subq4"."c0")))) >= ? THEN (SELECT "$$users3"."age" FROM "users" AS "$$users3" WHERE ("$$users3"."id" = (SELECT "$$split1"."manager_id" FROM "$$split1" AS "$$split1" WHERE ("$$split1"."$$key" = "$$subq4"."c0")))) ELSE "$$panic"(?) END))"#,
    );
    assert_eq!(
        compiled.statements[2].params,
        vec![
            int_param(1),
            int_param(0),
            Param::Value(ParamValue::String(
                "take requires a non-negative integer".to_string()
            )),
        ]
    );
}

#[test]
fn nested_scalar_temp_arm() {
    let compiled = compile("users { o = orders { t = [age].first() } }");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$split1"."$$key" FROM "$$split1" AS "$$split1""#,
    );
    assert!(compiled.statements[1].params.is_empty());
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT (SELECT "lit3".* FROM (SELECT (SELECT "$$split1"."age" FROM "$$split1" AS "$$split1" WHERE ("$$split1"."$$key" = "$$orders2"."user_id")) AS "c0") AS "lit3" LIMIT ?) AS "t", "$$orders2"."user_id" FROM "orders" AS "$$orders2" WHERE ("$$orders2"."user_id" IN (SELECT "$$split1"."$$key" FROM "$$split1" AS "$$split1"))"#,
    );
    assert_eq!(compiled.statements[2].params, vec![int_param(1)]);
}

#[test]
fn nested_placing_temp_arm() {
    let compiled = compile("users { o = orders.take(age) { s = [total].first() } }");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$split1"."$$key" FROM "$$split1" AS "$$split1""#,
    );
    assert!(compiled.statements[1].params.is_empty());
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT (SELECT "lit4".* FROM (SELECT "$$subq3"."total" AS "c0") AS "lit4" LIMIT ?) AS "s", "$$subq3"."user_id" FROM (SELECT "$$orders2".*, "ROW_NUMBER"() OVER (PARTITION BY "$$orders2"."user_id") AS "$$rn" FROM "orders" AS "$$orders2" WHERE ("$$orders2"."user_id" IN (SELECT "$$split1"."$$key" FROM "$$split1" AS "$$split1"))) AS "$$subq3" WHERE ("$$subq3"."$$rn" <= (CASE WHEN (SELECT "$$split1"."age" FROM "$$split1" AS "$$split1" WHERE ("$$split1"."$$key" = "$$subq3"."user_id")) >= ? THEN (SELECT "$$split1"."age" FROM "$$split1" AS "$$split1" WHERE ("$$split1"."$$key" = "$$subq3"."user_id")) ELSE "$$panic"(?) END))"#,
    );
    assert_eq!(
        compiled.statements[2].params,
        vec![
            int_param(1),
            int_param(0),
            Param::Value(ParamValue::String(
                "take requires a non-negative integer".to_string()
            )),
        ]
    );
}

#[test]
fn tuple_item_scalars() {
    let (sql, params) = sql("users { t = (1, 2) }");
    assert_sql_eq(
        &sql,
        r#"SELECT ? AS "t_c0", ? AS "t_c1" FROM "users" AS "$$users0""#,
    );
    assert_eq!(params, vec![int_param(1), int_param(2)]);
}

#[test]
fn tuple_item_field() {
    let (sql, params) = sql("users { p = pos }");
    assert_sql_eq(
        &sql,
        r#"SELECT "$$users0"."lat" AS "p_c0", "$$users0"."lng" AS "p_c1" FROM "users" AS "$$users0""#,
    );
    assert!(params.is_empty());
}

#[test]
fn tuple_field_result() {
    let (sql, params) = sql("users.pos");
    assert_sql_eq(
        &sql,
        r#"SELECT "$$users0"."lat", "$$users0"."lng" FROM "users" AS "$$users0""#,
    );
    assert!(params.is_empty());
}

#[test]
fn tuple_item_keyed_child() {
    let compiled = compile("users { t = (1, orders) }");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"SELECT ? AS "t_c0", "$$users0"."id" FROM "users" AS "$$users0""#,
    );
    assert_eq!(compiled.statements[0].params, vec![int_param(1)]);
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$orders1".* FROM "orders" AS "$$orders1" WHERE ("$$orders1"."user_id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"()))"#,
    );
}

#[test]
fn tuple_item_broadcast_children() {
    let compiled = compile("users { t = ([1], [2]) }");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"SELECT "$$users0".* FROM "users" AS "$$users0""#,
    );
}

#[test]
fn tuple_item_single() {
    let compiled = compile("users { t = (orders.first(), 1) }");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"SELECT ? AS "t_c1", "$$users0"."id" FROM "users" AS "$$users0""#,
    );
}

#[test]
fn tuple_result_with_list() {
    let compiled = compile(r#"(1, [2])"#);
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(&compiled.statements[0].sql, r#"SELECT ? AS "c0""#);
    assert_eq!(compiled.statements[0].params, vec![int_param(1)]);
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "lit0".* FROM "$$lit_1_I"('lit0') AS "lit0""#,
    );
}

#[test]
fn tuple_result_with_single() {
    let compiled = compile("(users.first(), 1)");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(&compiled.statements[0].sql, r#"SELECT ? AS "c1""#);
    assert_eq!(compiled.statements[0].params, vec![int_param(1)]);
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$users0".* FROM "users" AS "$$users0" LIMIT ?"#,
    );
    assert_eq!(compiled.statements[1].params, vec![int_param(1)]);
}

#[test]
fn tuple_result_nested() {
    let (sql, params) = sql("(1, (2, 3))");
    assert_sql_eq(&sql, r#"SELECT ? AS "c0", ? AS "c1_c0", ? AS "c1_c1""#);
    assert_eq!(params, vec![int_param(1), int_param(2), int_param(3)]);
}

#[test]
fn tuple_collapse_item() {
    let (sql, params) = sql("users { t = users.pos.first() }");
    assert_sql_eq(
        &sql,
        r#"SELECT (SELECT "$$users1"."lat" FROM "users" AS "$$users1" LIMIT ?) AS "t_c0", (SELECT "$$users1"."lng" FROM "users" AS "$$users1" LIMIT ?) AS "t_c1" FROM "users" AS "$$users0""#,
    );
    assert_eq!(params, vec![int_param(1), int_param(1)]);
}

#[test]
fn tuple_collapse_result() {
    let (sql, params) = sql("users.pos.first()");
    assert_sql_eq(
        &sql,
        r#"SELECT (SELECT "$$users0"."lat" FROM "users" AS "$$users0" LIMIT ?) AS "c0", (SELECT "$$users0"."lng" FROM "users" AS "$$users0" LIMIT ?) AS "c1""#,
    );
    assert_eq!(params, vec![int_param(1), int_param(1)]);
}

#[test]
fn tuple_per_row_element() {
    use crate::codegen::model::{Cardinality, ChildRef, ParentRef, ResultShape};
    let compiled = compile("(1, users.orders)");
    assert_eq!(
        compiled.result,
        ResultShape::Tuple(vec![
            ResultShape::Column("c0".to_string()),
            ResultShape::List {
                element: Box::new(ResultShape::List {
                    element: Box::new(ResultShape::Record(vec![(
                        "total".to_string(),
                        ResultShape::Column("total".to_string())
                    ),])),
                    child: ChildRef {
                        stmt: 2,
                        parent: Some(ParentRef {
                            stmt: 1,
                            parent_key: "id".to_string(),
                            child_key: "user_id".to_string(),
                            negated: false,
                        }),
                        cardinality: Cardinality::Many,
                        tag: None,
                    },
                }),
                child: ChildRef {
                    stmt: 1,
                    parent: None,
                    cardinality: Cardinality::Many,
                    tag: None,
                },
            },
        ]),
    );
}

#[test]
fn tuple_nested_element() {
    use crate::codegen::model::{Cardinality, ChildRef, ParentRef, ResultShape};
    let compiled = compile("(1, [[2]])");
    assert_eq!(
        compiled.result,
        ResultShape::Tuple(vec![
            ResultShape::Column("c0".to_string()),
            ResultShape::List {
                element: Box::new(ResultShape::List {
                    element: Box::new(ResultShape::Column("c1".to_string())),
                    child: ChildRef {
                        stmt: 2,
                        parent: Some(ParentRef {
                            stmt: 1,
                            parent_key: "c0".to_string(),
                            child_key: "c0".to_string(),
                            negated: false,
                        }),
                        cardinality: Cardinality::Many,
                        tag: None,
                    },
                }),
                child: ChildRef {
                    stmt: 1,
                    parent: None,
                    cardinality: Cardinality::Many,
                    tag: None,
                },
            },
        ]),
    );
}

#[test]
fn if_list_merge() {
    let (sql, params) = sql("if true { [1] } else { [2] }");
    assert_sql_eq(
        &sql,
        r#"SELECT "lit0".* FROM "$$lit_1_I"('lit0') AS "lit0""#,
    );
    assert_eq!(params, vec![]);
}

#[test]
fn if_else_if_chain() {
    let (sql, params) = sql("if false { [1] } else if true { [2] } else { [3] }");
    assert_sql_eq(
        &sql,
        r#"SELECT "lit0".* FROM "$$lit_1_I"('lit0') AS "lit0""#,
    );
    assert_eq!(params, vec![]);
}

#[test]
fn if_storage_branches() {
    let (guard_sql, guard_params) = case_pos_guard(1, "take");
    let (sql, params) = sql("if 1 > 2 { users } else { users.take(1) }");
    assert_sql_eq(
        &sql,
        &format!(
            r#"SELECT "$$users0".* FROM "users" AS "$$users0" WHERE (? > ?) UNION ALL SELECT "$$users1".* FROM "users" AS "$$users1" WHERE NOT (? > ?) LIMIT ({guard_sql})"#
        ),
    );
    let mut expected = vec![int_param(1), int_param(2), int_param(1), int_param(2)];
    expected.extend(guard_params);
    assert_eq!(params, expected);
}

#[test]
fn if_take_over_union() {
    let (guard_sql, guard_params) = case_pos_guard(1, "take");
    let (sql, params) = sql("if true { [1, 2] } else { [3] }.take(1)");
    assert_sql_eq(
        &sql,
        &format!(r#"SELECT "lit0".* FROM "$$lit_1_I"('lit0') AS "lit0" LIMIT ({guard_sql})"#),
    );
    assert_eq!(params, guard_params);
}

#[test]
fn if_tuple_distribute_result() {
    let (sql, params) = sql("if true { (1, 2) } else { (3, 4) }");
    assert_sql_eq(
        &sql,
        r#"SELECT (CASE WHEN ? THEN ? ELSE ? END) AS "c0", (CASE WHEN ? THEN ? ELSE ? END) AS "c1""#,
    );
    assert_eq!(
        params,
        vec![
            Param::Value(ParamValue::Bool(true)),
            int_param(1),
            int_param(3),
            Param::Value(ParamValue::Bool(true)),
            int_param(2),
            int_param(4),
        ]
    );
}

#[test]
fn if_tuple_nested_distribute_result() {
    let (sql, params) = sql("if true { if false { (1, 2) } else { (3, 4) } } else { (5, 6) }");
    assert_sql_eq(
        &sql,
        r#"SELECT (CASE WHEN ? THEN CASE WHEN ? THEN ? ELSE ? END ELSE ? END) AS "c0", (CASE WHEN ? THEN CASE WHEN ? THEN ? ELSE ? END ELSE ? END) AS "c1""#,
    );
    assert_eq!(
        params,
        vec![
            Param::Value(ParamValue::Bool(true)),
            Param::Value(ParamValue::Bool(false)),
            int_param(1),
            int_param(3),
            int_param(5),
            Param::Value(ParamValue::Bool(true)),
            Param::Value(ParamValue::Bool(false)),
            int_param(2),
            int_param(4),
            int_param(6),
        ]
    );
}

#[test]
fn if_tuple_distribute_item() {
    let (sql, params) = sql("users { t = if active { (1, 2) } else { (3, 4) } }");
    assert_sql_eq(
        &sql,
        r#"SELECT (CASE WHEN "$$users0"."active" THEN ? ELSE ? END) AS "t_c0", (CASE WHEN "$$users0"."active" THEN ? ELSE ? END) AS "t_c1" FROM "users" AS "$$users0""#,
    );
    assert_eq!(
        params,
        vec![int_param(1), int_param(3), int_param(2), int_param(4)]
    );
}

#[test]
fn if_struct_distribute_result() {
    let (sql, params) = sql("if true { { a = 1 } } else { { a = 2 } }");
    assert_sql_eq(&sql, r#"SELECT (CASE WHEN ? THEN ? ELSE ? END) AS "a""#);
    assert_eq!(
        params,
        vec![
            Param::Value(ParamValue::Bool(true)),
            int_param(1),
            int_param(2),
        ]
    );
}

#[test]
fn if_correlated_item() {
    use crate::codegen::model::{Cardinality, ChildRef, ParentRef, ResultShape};
    let compiled = compile("users { x = if active { [1] } else { [2] } }");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"CREATE TEMP TABLE "$$split1" AS SELECT "$$users0".*, "ROW_NUMBER"() OVER () AS "$$key" FROM "users" AS "$$users0""#,
    );
    assert!(compiled.statements[0].params.is_empty());
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT ? AS "c0", "$$split1"."$$key" AS "$$pkey" FROM "$$split1" AS "$$split1" WHERE "$$split1"."active" UNION ALL SELECT ? AS "c0", "$$split1"."$$key" AS "$$pkey" FROM "$$split1" AS "$$split1" WHERE NOT "$$split1"."active""#,
    );
    assert_eq!(
        compiled.statements[2].params,
        vec![int_param(1), int_param(2)]
    );
    assert_eq!(
        compiled.result,
        ResultShape::List {
            element: Box::new(ResultShape::Record(vec![(
                "x".to_string(),
                ResultShape::List {
                    element: Box::new(ResultShape::Column("c0".to_string())),
                    child: ChildRef {
                        stmt: 2,
                        parent: Some(ParentRef {
                            stmt: 0,
                            parent_key: "$$key".to_string(),
                            child_key: "$$pkey".to_string(),
                            negated: false,
                        }),
                        cardinality: Cardinality::Many,
                        tag: None,
                    },
                },
            )])),
            child: ChildRef {
                stmt: 1,
                parent: None,
                cardinality: Cardinality::Many,
                tag: None,
            },
        }
    );
}

#[test]
fn if_record_item() {
    use crate::codegen::model::{Cardinality, ChildRef, ParentRef, ResultShape};
    let compiled = compile("users { o = if active { users.first() } else { users.first() } }");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"CREATE TEMP TABLE "$$split3" AS SELECT "$$users0".*, "ROW_NUMBER"() OVER () AS "$$key" FROM "users" AS "$$users0""#,
    );
    assert!(compiled.statements[0].params.is_empty());
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$subq4"."name", "$$subq4"."address", "$$subq4"."age", "$$subq4"."score", "$$subq4"."balance", "$$subq4"."active", "$$subq4"."nickname", "$$subq4"."lat", "$$subq4"."lng", "$$subq4"."t0", "$$subq4"."t1", "$$subq4"."t2", "$$subq4"."created", "$$subq4"."ttl", "$$subq4"."id", "$$subq4"."manager_id", "$$subq4"."mentor_id", "$$split3"."$$key" AS "$$pkey" FROM "$$split3" AS "$$split3", (SELECT "$$users1".* FROM "users" AS "$$users1" LIMIT ?) AS "$$subq4" WHERE "$$split3"."active" UNION ALL SELECT "$$subq5"."name", "$$subq5"."address", "$$subq5"."age", "$$subq5"."score", "$$subq5"."balance", "$$subq5"."active", "$$subq5"."nickname", "$$subq5"."lat", "$$subq5"."lng", "$$subq5"."t0", "$$subq5"."t1", "$$subq5"."t2", "$$subq5"."created", "$$subq5"."ttl", "$$subq5"."id", "$$subq5"."manager_id", "$$subq5"."mentor_id", "$$split3"."$$key" AS "$$pkey" FROM "$$split3" AS "$$split3", (SELECT "$$users2".* FROM "users" AS "$$users2" LIMIT ?) AS "$$subq5" WHERE NOT "$$split3"."active""#,
    );
    assert_eq!(
        compiled.result,
        ResultShape::List {
            element: Box::new(ResultShape::Record(vec![(
                "o".to_string(),
                ResultShape::Single {
                    element: Box::new(ResultShape::Record(vec![
                        ("name".to_string(), ResultShape::Column("name".to_string())),
                        (
                            "address".to_string(),
                            ResultShape::Column("address".to_string())
                        ),
                        ("age".to_string(), ResultShape::Column("age".to_string())),
                        (
                            "score".to_string(),
                            ResultShape::Column("score".to_string())
                        ),
                        (
                            "balance".to_string(),
                            ResultShape::Column("balance".to_string())
                        ),
                        (
                            "active".to_string(),
                            ResultShape::Column("active".to_string())
                        ),
                        (
                            "nickname".to_string(),
                            ResultShape::Column("nickname".to_string())
                        ),
                        ("lat".to_string(), ResultShape::Column("lat".to_string())),
                        ("lng".to_string(), ResultShape::Column("lng".to_string())),
                        ("t0".to_string(), ResultShape::Column("t0".to_string())),
                        ("t1".to_string(), ResultShape::Column("t1".to_string())),
                        ("t2".to_string(), ResultShape::Column("t2".to_string())),
                        (
                            "created".to_string(),
                            ResultShape::Column("created".to_string())
                        ),
                        ("ttl".to_string(), ResultShape::Column("ttl".to_string())),
                    ])),
                    child: ChildRef {
                        stmt: 2,
                        parent: Some(ParentRef {
                            stmt: 0,
                            parent_key: "$$key".to_string(),
                            child_key: "$$pkey".to_string(),
                            negated: false,
                        }),
                        cardinality: Cardinality::One,
                        tag: None,
                    },
                },
            )])),
            child: ChildRef {
                stmt: 1,
                parent: None,
                cardinality: Cardinality::Many,
                tag: None,
            },
        }
    );
}

#[test]
fn if_multi_statement_nesting() {
    use crate::codegen::model::{Cardinality, ChildRef, ParentRef, ResultShape};
    let compiled = compile("if true { users.orders } else { users.orders }");
    let inner = ChildRef {
        stmt: 1,
        parent: Some(ParentRef {
            stmt: 0,
            parent_key: "id".to_string(),
            child_key: "user_id".to_string(),
            negated: false,
        }),
        cardinality: Cardinality::Many,
        tag: None,
    };
    assert_eq!(
        compiled.result,
        ResultShape::List {
            element: Box::new(ResultShape::List {
                element: Box::new(ResultShape::Record(vec![(
                    "total".to_string(),
                    ResultShape::Column("total".to_string())
                ),])),
                child: inner.clone(),
            }),
            child: inner,
        }
    );
}

#[test]
fn cross_correlated_filter() {
    let compiled = compile("users { x = configs.filter(owner == email) }");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"SELECT "$$users0"."address" FROM "users" AS "$$users0""#,
    );
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$configs1".*, (SELECT "$$users2"."address" FROM "users" AS "$$users2" WHERE ("$$users2"."address" = "$$configs1"."owner")) AS "$$pkey" FROM "configs" AS "$$configs1""#,
    );
}

#[test]
fn cross_uncorrelated_broadcast() {
    let compiled = compile("configs { u = users.filter(age > 1) }");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"SELECT "$$configs0".* FROM "configs" AS "$$configs0""#,
    );
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$users1".* FROM "users" AS "$$users1" WHERE ("$$users1"."age" > ?)"#,
    );
    assert_eq!(compiled.statements[1].params, vec![int_param(1)]);
}

#[test]
fn cross_disjunctive() {
    use crate::codegen::model::{Cardinality, ChildRef, ParentRef, ResultShape};
    let compiled = compile(r#"users { x = configs.filter(owner == email || name == "x") }"#);
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"SELECT "$$users0"."address" FROM "users" AS "$$users0""#,
    );
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$configs1".*, (SELECT "$$users2"."address" FROM "users" AS "$$users2" WHERE (("$$users2"."address" = "$$configs1"."owner") AND NOT ("$$configs1"."name" = ?))) AS "$$pkey", ? AS "$$ptag" FROM "configs" AS "$$configs1" UNION ALL SELECT "$$configs1".*, ? AS "$$pkey", ? AS "$$ptag" FROM "configs" AS "$$configs1" WHERE ("$$configs1"."name" = ?)"#,
    );
    assert_eq!(
        compiled.statements[1].params,
        vec![
            str_param("x"),
            int_param(0),
            Param::Value(ParamValue::Null),
            int_param(1),
            str_param("x"),
        ]
    );
    assert_eq!(
        compiled.result,
        ResultShape::List {
            element: Box::new(ResultShape::Record(vec![(
                "x".to_string(),
                ResultShape::List {
                    element: Box::new(ResultShape::Record(vec![
                        ("name".to_string(), ResultShape::Column("name".to_string())),
                        (
                            "owner".to_string(),
                            ResultShape::Column("owner".to_string())
                        ),
                    ])),
                    child: ChildRef {
                        stmt: 1,
                        parent: Some(ParentRef {
                            stmt: 0,
                            parent_key: "address".to_string(),
                            child_key: "$$pkey".to_string(),
                            negated: false,
                        }),
                        cardinality: Cardinality::Many,
                        tag: None,
                    },
                },
            )])),
            child: ChildRef {
                stmt: 0,
                parent: None,
                cardinality: Cardinality::Many,
                tag: None,
            },
        }
    );
}

#[test]
fn cross_disjunctive_shared_column() {
    let compiled = compile("users { x = configs.filter(owner == email || name == email) }");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$configs1".*, (SELECT "$$users2"."address" FROM "users" AS "$$users2" WHERE (("$$users2"."address" = "$$configs1"."owner") OR ("$$users2"."address" = "$$configs1"."name"))) AS "$$pkey" FROM "configs" AS "$$configs1""#,
    );
    assert!(compiled.statements[1].params.is_empty());
}

#[test]
fn cross_disjunctive_temp_parent() {
    let compiled =
        compile("users.filter(age > 35) { x = configs.filter(owner == email || name == email) }");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$configs2".*, (SELECT "$$split1"."address" FROM "$$split1" AS "$$split1" WHERE (("$$split1"."address" = "$$configs2"."owner") OR ("$$split1"."address" = "$$configs2"."name"))) AS "$$pkey" FROM "configs" AS "$$configs2""#,
    );
    assert_eq!(compiled.statements[0].params, vec![int_param(35)]);
    assert!(compiled.statements[2].params.is_empty());
}

#[test]
fn cross_per_row_take() {
    use crate::codegen::model::{Cardinality, ChildRef, ParentRef, ResultShape};
    let compiled = compile("users { x = configs.take(age) }");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"CREATE TEMP TABLE "$$split1" AS SELECT "$$users0".*, "ROW_NUMBER"() OVER () AS "$$key" FROM "users" AS "$$users0""#,
    );
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$subq4".* FROM (SELECT "$$subq3".*, "ROW_NUMBER"() OVER (PARTITION BY "$$subq3"."$$pkey") AS "$$rn" FROM (SELECT "$$configs2"."name", "$$configs2"."owner", "$$split1"."$$key" AS "$$pkey", "$$split1"."age" AS "$$tbound" FROM "$$split1" AS "$$split1", "configs" AS "$$configs2") AS "$$subq3") AS "$$subq4" WHERE ("$$subq4"."$$rn" <= (CASE WHEN "$$subq4"."$$tbound" >= ? THEN "$$subq4"."$$tbound" ELSE "$$panic"(?) END))"#,
    );
    assert_eq!(
        compiled.statements[2].params,
        vec![
            int_param(0),
            Param::Value(ParamValue::String(
                "take requires a non-negative integer".to_string()
            )),
        ]
    );
    assert_eq!(
        compiled.result,
        ResultShape::List {
            element: Box::new(ResultShape::Record(vec![(
                "x".to_string(),
                ResultShape::List {
                    element: Box::new(ResultShape::Record(vec![
                        ("name".to_string(), ResultShape::Column("name".to_string())),
                        (
                            "owner".to_string(),
                            ResultShape::Column("owner".to_string())
                        ),
                    ])),
                    child: ChildRef {
                        stmt: 2,
                        parent: Some(ParentRef {
                            stmt: 0,
                            parent_key: "$$key".to_string(),
                            child_key: "$$pkey".to_string(),
                            negated: false,
                        }),
                        cardinality: Cardinality::Many,
                        tag: None,
                    },
                },
            )])),
            child: ChildRef {
                stmt: 1,
                parent: None,
                cardinality: Cardinality::Many,
                tag: None,
            },
        }
    );
}

#[test]
fn cross_per_row_take_narrow_parent() {
    let compiled = compile("users { name, x = configs.take(age) }");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"CREATE TEMP TABLE "$$split1" AS SELECT "$$users0".*, "ROW_NUMBER"() OVER () AS "$$key" FROM "users" AS "$$users0""#,
    );
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$split1"."name", "$$split1"."$$key" FROM "$$split1" AS "$$split1""#,
    );
}

#[test]
fn cross_per_row_skip() {
    let compiled = compile("users { x = configs.skip(age) }");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$subq4".* FROM (SELECT "$$subq3".*, "ROW_NUMBER"() OVER (PARTITION BY "$$subq3"."$$pkey") AS "$$rn" FROM (SELECT "$$configs2"."name", "$$configs2"."owner", "$$split1"."$$key" AS "$$pkey", "$$split1"."age" AS "$$tbound" FROM "$$split1" AS "$$split1", "configs" AS "$$configs2") AS "$$subq3") AS "$$subq4" WHERE ("$$subq4"."$$rn" > (CASE WHEN "$$subq4"."$$tbound" >= ? THEN "$$subq4"."$$tbound" ELSE "$$panic"(?) END))"#,
    );
}

#[test]
fn take_bound_navigation_over_temp() {
    use crate::codegen::model::{Cardinality, ChildRef, ParentRef, ResultShape};
    let compiled = compile("users { o = orders.take(manager.age) { total } }");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$subq4"."total", "$$subq4"."user_id" FROM (SELECT "$$orders2".*, "ROW_NUMBER"() OVER (PARTITION BY "$$orders2"."user_id") AS "$$rn" FROM "orders" AS "$$orders2" WHERE ("$$orders2"."user_id" IN (SELECT "$$split1"."$$key" FROM "$$split1" AS "$$split1"))) AS "$$subq4" WHERE ("$$subq4"."$$rn" <= (CASE WHEN (SELECT "$$users3"."age" FROM "users" AS "$$users3" WHERE ("$$users3"."id" = (SELECT "$$split1"."manager_id" FROM "$$split1" AS "$$split1" WHERE ("$$split1"."$$key" = "$$subq4"."user_id")))) >= ? THEN (SELECT "$$users3"."age" FROM "users" AS "$$users3" WHERE ("$$users3"."id" = (SELECT "$$split1"."manager_id" FROM "$$split1" AS "$$split1" WHERE ("$$split1"."$$key" = "$$subq4"."user_id")))) ELSE "$$panic"(?) END))"#,
    );
    assert_eq!(
        compiled.result,
        ResultShape::List {
            element: Box::new(ResultShape::Record(vec![(
                "o".to_string(),
                ResultShape::List {
                    element: Box::new(ResultShape::Record(vec![(
                        "total".to_string(),
                        ResultShape::Column("total".to_string())
                    ),])),
                    child: ChildRef {
                        stmt: 2,
                        parent: Some(ParentRef {
                            stmt: 0,
                            parent_key: "$$key".to_string(),
                            child_key: "user_id".to_string(),
                            negated: false,
                        }),
                        cardinality: Cardinality::Many,
                        tag: None,
                    },
                }
            ),])),
            child: ChildRef {
                stmt: 1,
                parent: None,
                cardinality: Cardinality::Many,
                tag: None,
            },
        }
    );
}

#[test]
fn skip_bound_navigation_over_temp() {
    let compiled = compile("users { o = orders.skip(manager.age) { total } }");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$subq4"."total", "$$subq4"."user_id" FROM (SELECT "$$orders2".*, "ROW_NUMBER"() OVER (PARTITION BY "$$orders2"."user_id") AS "$$rn" FROM "orders" AS "$$orders2" WHERE ("$$orders2"."user_id" IN (SELECT "$$split1"."$$key" FROM "$$split1" AS "$$split1"))) AS "$$subq4" WHERE ("$$subq4"."$$rn" > (CASE WHEN (SELECT "$$users3"."age" FROM "users" AS "$$users3" WHERE ("$$users3"."id" = (SELECT "$$split1"."manager_id" FROM "$$split1" AS "$$split1" WHERE ("$$split1"."$$key" = "$$subq4"."user_id")))) >= ? THEN (SELECT "$$users3"."age" FROM "users" AS "$$users3" WHERE ("$$users3"."id" = (SELECT "$$split1"."manager_id" FROM "$$split1" AS "$$split1" WHERE ("$$split1"."$$key" = "$$subq4"."user_id")))) ELSE "$$panic"(?) END))"#,
    );
}

#[test]
fn cross_take_over_correlated() {
    use crate::codegen::model::{Cardinality, ChildRef, ParentRef, ResultShape};
    let compiled = compile("users { x = configs.filter(owner == email).take(1) }");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$subq4".* FROM (SELECT "$$subq3".*, "ROW_NUMBER"() OVER (PARTITION BY "$$subq3"."$$pkey") AS "$$rn" FROM (SELECT "$$configs1".*, (SELECT "$$users2"."address" FROM "users" AS "$$users2" WHERE ("$$users2"."address" = "$$configs1"."owner")) AS "$$pkey" FROM "configs" AS "$$configs1") AS "$$subq3") AS "$$subq4" WHERE ("$$subq4"."$$rn" <= (CASE WHEN ? >= ? THEN ? ELSE "$$panic"(?) END))"#,
    );
    assert_eq!(
        compiled.statements[1].params,
        vec![int_param(1), int_param(0), int_param(1), limit_msg("take")]
    );
    assert_eq!(
        compiled.result,
        ResultShape::List {
            element: Box::new(ResultShape::Record(vec![(
                "x".to_string(),
                ResultShape::List {
                    element: Box::new(ResultShape::Record(vec![
                        ("name".to_string(), ResultShape::Column("name".to_string())),
                        (
                            "owner".to_string(),
                            ResultShape::Column("owner".to_string())
                        ),
                    ])),
                    child: ChildRef {
                        stmt: 1,
                        parent: Some(ParentRef {
                            stmt: 0,
                            parent_key: "address".to_string(),
                            child_key: "$$pkey".to_string(),
                            negated: false,
                        }),
                        cardinality: Cardinality::Many,
                        tag: None,
                    },
                },
            )])),
            child: ChildRef {
                stmt: 0,
                parent: None,
                cardinality: Cardinality::Many,
                tag: None,
            },
        }
    );
}

#[test]
fn cross_take_under_correlated_filter_applies_globally() {
    use crate::codegen::model::{Cardinality, ChildRef, ParentRef, ResultShape};
    let compiled = compile("users { x = configs.take(1).filter(owner == email) }");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$configs1".*, (SELECT "$$users2"."address" FROM "users" AS "$$users2" WHERE ("$$users2"."address" = "$$configs1"."owner")) AS "$$pkey" FROM "configs" AS "$$configs1" LIMIT (CASE WHEN ? >= ? THEN ? ELSE "$$panic"(?) END)"#,
    );
    assert_eq!(
        compiled.statements[1].params,
        vec![int_param(1), int_param(0), int_param(1), limit_msg("take")]
    );
    assert_eq!(
        compiled.result,
        ResultShape::List {
            element: Box::new(ResultShape::Record(vec![(
                "x".to_string(),
                ResultShape::List {
                    element: Box::new(ResultShape::Record(vec![
                        ("name".to_string(), ResultShape::Column("name".to_string())),
                        (
                            "owner".to_string(),
                            ResultShape::Column("owner".to_string())
                        ),
                    ])),
                    child: ChildRef {
                        stmt: 1,
                        parent: Some(ParentRef {
                            stmt: 0,
                            parent_key: "address".to_string(),
                            child_key: "$$pkey".to_string(),
                            negated: false,
                        }),
                        cardinality: Cardinality::Many,
                        tag: None,
                    },
                },
            )])),
            child: ChildRef {
                stmt: 0,
                parent: None,
                cardinality: Cardinality::Many,
                tag: None,
            },
        }
    );
}

#[test]
fn cross_take_sort_materializes_boundary() {
    let compiled = compile("users { x = configs.take(1).sort(name).filter(owner == email) }");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$subq3".*, (SELECT "$$users4"."address" FROM "users" AS "$$users4" WHERE ("$$users4"."address" = "$$subq3"."owner")) AS "$$pkey" FROM (SELECT "$$configs1".* FROM "configs" AS "$$configs1" LIMIT (CASE WHEN ? >= ? THEN ? ELSE "$$panic"(?) END)) AS "$$subq3" ORDER BY "$$subq3"."name" ASC"#,
    );
    assert_eq!(
        compiled.statements[1].params,
        vec![int_param(1), int_param(0), int_param(1), limit_msg("take")]
    );
}

#[test]
fn cross_skip_over_correlated() {
    let compiled = compile("users { x = configs.filter(owner == email).skip(1) }");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$subq4".* FROM (SELECT "$$subq3".*, "ROW_NUMBER"() OVER (PARTITION BY "$$subq3"."$$pkey") AS "$$rn" FROM (SELECT "$$configs1".*, (SELECT "$$users2"."address" FROM "users" AS "$$users2" WHERE ("$$users2"."address" = "$$configs1"."owner")) AS "$$pkey" FROM "configs" AS "$$configs1") AS "$$subq3") AS "$$subq4" WHERE ("$$subq4"."$$rn" > (CASE WHEN ? >= ? THEN ? ELSE "$$panic"(?) END))"#,
    );
    assert_eq!(
        compiled.statements[1].params,
        vec![int_param(1), int_param(0), int_param(1), limit_msg("skip")]
    );
}

#[test]
fn cross_nav_unwrap_or_coalesce() {
    let compiled =
        compile(r#"users { x = configs.filter(owner == mentor?.email.unwrap_or("zzz")) }"#);
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"CREATE TEMP TABLE "$$split1" AS SELECT "$$users0".*, COALESCE((SELECT "$$users2"."address" FROM "users" AS "$$users2" WHERE ("$$users2"."id" = "$$users0"."mentor_id")), ?) AS "$$nav3" FROM "users" AS "$$users0""#,
    );
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$configs1".*, (SELECT "$$split2"."$$nav3" FROM "$$split2" AS "$$split2" WHERE ("$$split2"."$$nav3" = "$$configs1"."owner")) AS "$$pkey" FROM "configs" AS "$$configs1""#,
    );
    assert_eq!(compiled.statements[0].params, vec![str_param("zzz")],);
}

#[test]
fn cross_sort_mixed() {
    let compiled = compile("users { x = configs.sort(name == email) }");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"CREATE TEMP TABLE "$$split1" AS SELECT "$$users0".*, "ROW_NUMBER"() OVER () AS "$$key" FROM "users" AS "$$users0""#,
    );
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$configs2"."name", "$$configs2"."owner", "$$split1"."$$key" AS "$$pkey" FROM "$$split1" AS "$$split1", "configs" AS "$$configs2" ORDER BY "$$configs2"."name" = "$$split1"."address" ASC"#,
    );
    assert!(compiled.statements[2].params.is_empty());
}

#[test]
fn cross_sorted_take_window() {
    let compiled = compile("users { x = configs.filter(owner == email).sort(name).take(1) }");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$subq4".* FROM (SELECT "$$subq3".*, "ROW_NUMBER"() OVER (PARTITION BY "$$subq3"."$$pkey" ORDER BY "$$subq3"."name" ASC) AS "$$rn" FROM (SELECT "$$configs1".*, (SELECT "$$users2"."address" FROM "users" AS "$$users2" WHERE ("$$users2"."address" = "$$configs1"."owner")) AS "$$pkey" FROM "configs" AS "$$configs1") AS "$$subq3") AS "$$subq4" WHERE ("$$subq4"."$$rn" <= (CASE WHEN ? >= ? THEN ? ELSE "$$panic"(?) END))"#,
    );
}

#[test]
fn cross_proj_take_over_correlated() {
    let compiled = compile("users { n = name } { x = configs.filter(owner == n).take(1) }");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"CREATE TEMP TABLE "$$split1" AS SELECT "$$users0"."name" AS "n" FROM "users" AS "$$users0""#,
    );
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$subq4".* FROM (SELECT "$$subq3".*, "ROW_NUMBER"() OVER (PARTITION BY "$$subq3"."$$pkey") AS "$$rn" FROM (SELECT "$$configs2".*, (SELECT "$$split1"."n" FROM "$$split1" AS "$$split1" WHERE ("$$split1"."n" = "$$configs2"."owner")) AS "$$pkey" FROM "configs" AS "$$configs2") AS "$$subq3") AS "$$subq4" WHERE ("$$subq4"."$$rn" <= (CASE WHEN ? >= ? THEN ? ELSE "$$panic"(?) END))"#,
    );
}

#[test]
fn cross_proj_corr_bare() {
    let compiled = compile("users { n = name } { x = configs.filter(owner == n) }");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"CREATE TEMP TABLE "$$split1" AS SELECT "$$users0"."name" AS "n" FROM "users" AS "$$users0""#,
    );
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$configs2".*, (SELECT "$$split1"."n" FROM "$$split1" AS "$$split1" WHERE ("$$split1"."n" = "$$configs2"."owner")) AS "$$pkey" FROM "configs" AS "$$configs2""#,
    );
}

#[test]
fn cross_inequality_anti_pairs() {
    use crate::codegen::model::{Cardinality, ChildRef, ParentRef, ResultShape};
    let compiled = compile("users { x = configs.filter(owner != email) }");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$configs1".*, (SELECT "$$users2"."address" FROM "users" AS "$$users2" WHERE ("$$users2"."address" = "$$configs1"."owner")) AS "$$pkey" FROM "configs" AS "$$configs1""#,
    );
    assert!(compiled.statements.iter().all(|s| s.params.is_empty()));
    assert_eq!(
        compiled.result,
        ResultShape::List {
            element: Box::new(ResultShape::Record(vec![(
                "x".to_string(),
                ResultShape::List {
                    element: Box::new(ResultShape::Record(vec![
                        ("name".to_string(), ResultShape::Column("name".to_string())),
                        (
                            "owner".to_string(),
                            ResultShape::Column("owner".to_string())
                        ),
                    ])),
                    child: ChildRef {
                        stmt: 1,
                        parent: Some(ParentRef {
                            stmt: 0,
                            parent_key: "address".to_string(),
                            child_key: "$$pkey".to_string(),
                            negated: true,
                        }),
                        cardinality: Cardinality::Many,
                        tag: None,
                    },
                },
            )])),
            child: ChildRef {
                stmt: 0,
                parent: None,
                cardinality: Cardinality::Many,
                tag: None,
            },
        }
    );
}

#[test]
fn cross_range_outer_right() {
    use crate::codegen::model::{Cardinality, ChildRef, ParentRef, ResultShape};
    let compiled = compile("users { x = configs.filter(owner > email) }");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"CREATE TEMP TABLE "$$split1" AS SELECT "$$users0".*, "ROW_NUMBER"() OVER () AS "$$key" FROM "users" AS "$$users0""#,
    );
    assert!(compiled.statements[0].params.is_empty());
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$configs1"."name", "$$configs1"."owner", "$$split1"."$$key" AS "$$pkey" FROM "$$split1" AS "$$split1", "configs" AS "$$configs1" WHERE ("$$split1"."address" < "$$configs1"."owner")"#,
    );
    assert!(compiled.statements[2].params.is_empty());
    assert_eq!(
        compiled.result,
        ResultShape::List {
            element: Box::new(ResultShape::Record(vec![(
                "x".to_string(),
                ResultShape::List {
                    element: Box::new(ResultShape::Record(vec![
                        ("name".to_string(), ResultShape::Column("name".to_string()),),
                        (
                            "owner".to_string(),
                            ResultShape::Column("owner".to_string()),
                        ),
                    ])),
                    child: ChildRef {
                        stmt: 2,
                        parent: Some(ParentRef {
                            stmt: 0,
                            parent_key: "$$key".to_string(),
                            child_key: "$$pkey".to_string(),
                            negated: false,
                        }),
                        cardinality: Cardinality::Many,
                        tag: None,
                    },
                },
            )])),
            child: ChildRef {
                stmt: 1,
                parent: None,
                cardinality: Cardinality::Many,
                tag: None,
            },
        }
    );
}

#[test]
fn cross_range_take_windows_matches() {
    use crate::codegen::model::{Cardinality, ChildRef, ParentRef, ResultShape};
    let compiled = compile("users { x = configs.filter(owner > email).take(2) }");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$subq4".* FROM (SELECT "$$subq3".*, "ROW_NUMBER"() OVER (PARTITION BY "$$subq3"."$$pkey") AS "$$rn" FROM (SELECT "$$configs1"."name", "$$configs1"."owner", "$$split1"."$$key" AS "$$pkey" FROM "$$split1" AS "$$split1", "configs" AS "$$configs1" WHERE ("$$split1"."address" < "$$configs1"."owner")) AS "$$subq3") AS "$$subq4" WHERE ("$$subq4"."$$rn" <= (CASE WHEN ? >= ? THEN ? ELSE "$$panic"(?) END))"#,
    );
    assert_eq!(
        compiled.statements[2].params,
        vec![int_param(2), int_param(0), int_param(2), limit_msg("take")]
    );
    assert_eq!(
        compiled.result,
        ResultShape::List {
            element: Box::new(ResultShape::Record(vec![(
                "x".to_string(),
                ResultShape::List {
                    element: Box::new(ResultShape::Record(vec![
                        ("name".to_string(), ResultShape::Column("name".to_string()),),
                        (
                            "owner".to_string(),
                            ResultShape::Column("owner".to_string()),
                        ),
                    ])),
                    child: ChildRef {
                        stmt: 2,
                        parent: Some(ParentRef {
                            stmt: 0,
                            parent_key: "$$key".to_string(),
                            child_key: "$$pkey".to_string(),
                            negated: false,
                        }),
                        cardinality: Cardinality::Many,
                        tag: None,
                    },
                },
            )])),
            child: ChildRef {
                stmt: 1,
                parent: None,
                cardinality: Cardinality::Many,
                tag: None,
            },
        }
    );
}

#[test]
fn correlated_mixed_sort_orders() {
    let compiled = compile("users { x = configs.filter(owner == email).sort(name == email) }");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$configs1"."name", "$$configs1"."owner", "$$split1"."$$key" AS "$$pkey" FROM "$$split1" AS "$$split1", "configs" AS "$$configs1" WHERE ("$$split1"."address" = "$$configs1"."owner") ORDER BY "$$configs1"."name" = "$$split1"."address" ASC"#,
    );
    assert!(compiled.statements[2].params.is_empty());
}

#[test]
fn correlated_outer_sort_orders() {
    let compiled = compile("users { x = configs.filter(owner == email).sort(owner == email) }");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$configs1"."name", "$$configs1"."owner", "$$split1"."$$key" AS "$$pkey" FROM "$$split1" AS "$$split1", "configs" AS "$$configs1" WHERE ("$$split1"."address" = "$$configs1"."owner") ORDER BY "$$configs1"."owner" = "$$split1"."address" ASC"#,
    );
    assert!(compiled.statements[2].params.is_empty());
}

#[test]
fn sort_method_outer_key() {
    let compiled = compile("users { x = configs.sort(email.len()) }");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$configs1"."name", "$$configs1"."owner", "$$split1"."$$key" AS "$$pkey" FROM "$$split1" AS "$$split1", "configs" AS "$$configs1" ORDER BY "length"("$$split1"."address") ASC"#,
    );
    assert!(compiled.statements[2].params.is_empty());
}

#[test]
fn sort_method_mixed_key() {
    let compiled = compile("users { x = configs.sort(name.len() + email.len()) }");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$configs1"."name", "$$configs1"."owner", "$$split1"."$$key" AS "$$pkey" FROM "$$split1" AS "$$split1", "configs" AS "$$configs1" ORDER BY "$$add"("length"("$$configs1"."name"), "length"("$$split1"."address")) ASC"#,
    );
    assert!(compiled.statements[2].params.is_empty());
}

#[test]
fn ne_take_windows_complement() {
    use crate::codegen::model::{Cardinality, ChildRef, ParentRef, ResultShape};
    let compiled = compile("users { x = configs.filter(owner != email).take(1) }");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$subq4".* FROM (SELECT "$$subq3".*, "ROW_NUMBER"() OVER (PARTITION BY "$$subq3"."$$pkey") AS "$$rn" FROM (SELECT "$$configs1"."name", "$$configs1"."owner", "$$split1"."$$key" AS "$$pkey" FROM "$$split1" AS "$$split1", "configs" AS "$$configs1" WHERE ("$$split1"."address" != "$$configs1"."owner")) AS "$$subq3") AS "$$subq4" WHERE ("$$subq4"."$$rn" <= (CASE WHEN ? >= ? THEN ? ELSE "$$panic"(?) END))"#,
    );
    assert_eq!(
        compiled.statements[2].params,
        vec![int_param(1), int_param(0), int_param(1), limit_msg("take")]
    );
    assert_eq!(
        compiled.result,
        ResultShape::List {
            element: Box::new(ResultShape::Record(vec![(
                "x".to_string(),
                ResultShape::List {
                    element: Box::new(ResultShape::Record(vec![
                        ("name".to_string(), ResultShape::Column("name".to_string()),),
                        (
                            "owner".to_string(),
                            ResultShape::Column("owner".to_string()),
                        ),
                    ])),
                    child: ChildRef {
                        stmt: 2,
                        parent: Some(ParentRef {
                            stmt: 0,
                            parent_key: "$$key".to_string(),
                            child_key: "$$pkey".to_string(),
                            negated: false,
                        }),
                        cardinality: Cardinality::Many,
                        tag: None,
                    },
                },
            )])),
            child: ChildRef {
                stmt: 1,
                parent: None,
                cardinality: Cardinality::Many,
                tag: None,
            },
        }
    );
}

#[test]
fn multi_conjunct_predicate_ands() {
    let compiled = compile("users { x = configs.filter(owner > email && owner != \"n1\") }");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$configs1"."name", "$$configs1"."owner", "$$split1"."$$key" AS "$$pkey" FROM "$$split1" AS "$$split1", "configs" AS "$$configs1" WHERE ("$$configs1"."owner" != ?) AND ("$$split1"."address" < "$$configs1"."owner")"#,
    );
    assert_eq!(compiled.statements[2].params, vec![str_param("n1")]);
}

#[test]
fn ne_broadcast_take_truncates() {
    use crate::codegen::model::{Cardinality, ChildRef, ParentRef, ResultShape};
    let compiled =
        compile("users { x = configs.filter(owner != email || name == \"n1\").take(1) }");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$configs1"."name", "$$configs1"."owner", "$$split1"."$$key" AS "$$pkey", ? AS "$$ptag" FROM "$$split1" AS "$$split1", "configs" AS "$$configs1" WHERE (("$$split1"."address" != "$$configs1"."owner") AND NOT ("$$configs1"."name" = ?)) UNION ALL SELECT "$$configs1"."name", "$$configs1"."owner", ? AS "$$pkey", ? AS "$$ptag" FROM "$$split1" AS "$$split1", "configs" AS "$$configs1" WHERE ("$$configs1"."name" = ?)"#,
    );
    assert_eq!(
        compiled.statements[2].params,
        vec![
            int_param(0),
            str_param("n1"),
            Param::Value(ParamValue::Null),
            int_param(1),
            str_param("n1"),
        ]
    );
    assert_eq!(
        compiled.result,
        ResultShape::List {
            element: Box::new(ResultShape::Record(vec![(
                "x".to_string(),
                ResultShape::List {
                    element: Box::new(ResultShape::Record(vec![
                        ("name".to_string(), ResultShape::Column("name".to_string()),),
                        (
                            "owner".to_string(),
                            ResultShape::Column("owner".to_string()),
                        ),
                    ])),
                    child: ChildRef {
                        stmt: 2,
                        parent: Some(ParentRef {
                            stmt: 0,
                            parent_key: "$$key".to_string(),
                            child_key: "$$pkey".to_string(),
                            negated: false,
                        }),
                        cardinality: Cardinality::Take {
                            offset: 0,
                            limit: Some(1),
                        },
                        tag: None,
                    },
                },
            )])),
            child: ChildRef {
                stmt: 1,
                parent: None,
                cardinality: Cardinality::Many,
                tag: None,
            },
        }
    );
}

#[test]
fn cross_range_broadcast_take_truncates() {
    use crate::codegen::model::{Cardinality, ChildRef, ParentRef, ResultShape};
    let compiled = compile("users { x = configs.filter(owner > email || name == \"n1\").take(2) }");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$configs1"."name", "$$configs1"."owner", "$$split1"."$$key" AS "$$pkey", ? AS "$$ptag" FROM "$$split1" AS "$$split1", "configs" AS "$$configs1" WHERE (("$$split1"."address" < "$$configs1"."owner") AND NOT ("$$configs1"."name" = ?)) UNION ALL SELECT "$$configs1"."name", "$$configs1"."owner", ? AS "$$pkey", ? AS "$$ptag" FROM "$$split1" AS "$$split1", "configs" AS "$$configs1" WHERE ("$$configs1"."name" = ?)"#,
    );
    assert_eq!(
        compiled.statements[2].params,
        vec![
            int_param(0),
            str_param("n1"),
            Param::Value(ParamValue::Null),
            int_param(1),
            str_param("n1"),
        ]
    );
    assert_eq!(
        compiled.result,
        ResultShape::List {
            element: Box::new(ResultShape::Record(vec![(
                "x".to_string(),
                ResultShape::List {
                    element: Box::new(ResultShape::Record(vec![
                        ("name".to_string(), ResultShape::Column("name".to_string()),),
                        (
                            "owner".to_string(),
                            ResultShape::Column("owner".to_string()),
                        ),
                    ])),
                    child: ChildRef {
                        stmt: 2,
                        parent: Some(ParentRef {
                            stmt: 0,
                            parent_key: "$$key".to_string(),
                            child_key: "$$pkey".to_string(),
                            negated: false,
                        }),
                        cardinality: Cardinality::Take {
                            offset: 0,
                            limit: Some(2),
                        },
                        tag: None,
                    },
                },
            )])),
            child: ChildRef {
                stmt: 1,
                parent: None,
                cardinality: Cardinality::Many,
                tag: None,
            },
        }
    );
}

#[test]
fn cross_nav_outer() {
    use crate::codegen::model::{Cardinality, ChildRef, ParentRef, ResultShape};
    let compiled = compile("users { x = configs.filter(owner == manager.name) }");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"CREATE TEMP TABLE "$$split1" AS SELECT "$$users0".*, (SELECT "$$users1"."name" FROM "users" AS "$$users1" WHERE ("$$users1"."id" = "$$users0"."manager_id")) AS "$$nav1" FROM "users" AS "$$users0""#,
    );
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$configs2".*, (SELECT "$$split1"."$$nav1" FROM "$$split1" AS "$$split1" WHERE ("$$split1"."$$nav1" = "$$configs2"."owner")) AS "$$pkey" FROM "configs" AS "$$configs2""#,
    );
    assert!(compiled.statements[2].params.is_empty());
    assert_eq!(
        compiled.result,
        ResultShape::List {
            element: Box::new(ResultShape::Record(vec![(
                "x".to_string(),
                ResultShape::List {
                    element: Box::new(ResultShape::Record(vec![
                        ("name".to_string(), ResultShape::Column("name".to_string())),
                        (
                            "owner".to_string(),
                            ResultShape::Column("owner".to_string())
                        ),
                    ])),
                    child: ChildRef {
                        stmt: 2,
                        parent: Some(ParentRef {
                            stmt: 0,
                            parent_key: "$$nav1".to_string(),
                            child_key: "$$pkey".to_string(),
                            negated: false,
                        }),
                        cardinality: Cardinality::Many,
                        tag: None,
                    },
                },
            )])),
            child: ChildRef {
                stmt: 1,
                parent: None,
                cardinality: Cardinality::Many,
                tag: None,
            },
        }
    );
}

#[test]
fn struct_result_scalars() {
    let (sql, params) = sql(r#"{ name = "Ada", age = 30 }"#);
    assert_sql_eq(&sql, r#"SELECT ? AS "name", ? AS "age""#);
    assert_eq!(params, vec![str_param("Ada"), int_param(30)]);
}

#[test]
fn struct_result_with_list() {
    let compiled = compile("{ xs = [1, 2] }");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(&compiled.statements[0].sql, r#"SELECT ? AS "$$val1""#);
    assert_eq!(compiled.statements[0].params, vec![int_param(1)]);
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "lit0".* FROM "$$lit_1_I"('lit0') AS "lit0""#,
    );
}

#[test]
fn struct_result_with_single() {
    let compiled = compile("{ o = users.first() }");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(&compiled.statements[0].sql, r#"SELECT ? AS "$$val1""#);
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$users0".* FROM "users" AS "$$users0" LIMIT ?"#,
    );
    assert_eq!(compiled.statements[1].params, vec![int_param(1)]);
}

#[test]
fn struct_result_nested() {
    let (sql, params) = sql("{ a = 1, b = (2, 3) }");
    assert_sql_eq(&sql, r#"SELECT ? AS "a", ? AS "b_c0", ? AS "b_c1""#);
    assert_eq!(params, vec![int_param(1), int_param(2), int_param(3)]);
}

#[test]
fn struct_item() {
    let (sql, params) = sql("users { r = { n = name } }");
    assert_sql_eq(
        &sql,
        r#"SELECT "$$users0"."name" AS "r_n" FROM "users" AS "$$users0""#,
    );
    assert!(params.is_empty());
}

#[test]
fn struct_item_with_child() {
    let compiled = compile("users { r = { n = name, o = orders } }");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"SELECT "$$users0"."name" AS "r_n", "$$users0"."id" FROM "users" AS "$$users0""#,
    );
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$orders1".* FROM "orders" AS "$$orders1" WHERE ("$$orders1"."user_id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"()))"#,
    );
}

#[test]
fn struct_per_row_field() {
    use crate::codegen::model::{Cardinality, ChildRef, ParentRef, ResultShape};
    let compiled = compile("{ x = 1, y = users.orders }");
    assert_eq!(
        compiled.result,
        ResultShape::Record(vec![
            ("x".to_string(), ResultShape::Column("x".to_string()),),
            (
                "y".to_string(),
                ResultShape::List {
                    element: Box::new(ResultShape::List {
                        element: Box::new(ResultShape::Record(vec![(
                            "total".to_string(),
                            ResultShape::Column("total".to_string())
                        ),])),
                        child: ChildRef {
                            stmt: 2,
                            parent: Some(ParentRef {
                                stmt: 1,
                                parent_key: "id".to_string(),
                                child_key: "user_id".to_string(),
                                negated: false,
                            }),
                            cardinality: Cardinality::Many,
                            tag: None,
                        },
                    }),
                    child: ChildRef {
                        stmt: 1,
                        parent: None,
                        cardinality: Cardinality::Many,
                        tag: None,
                    },
                }
            ),
        ]),
    );
}

#[test]
fn take_over_nested_flatten_merges() {
    let (guard_sql, guard_params) = case_pos_guard(5, "take");
    let (sql, params) = sql("users.orders.take(5).flatten()");
    assert_sql_eq(
        &sql,
        &format!(
            r#"SELECT "$$orders1".* FROM "orders" AS "$$orders1" WHERE ("$$orders1"."user_id" IN (SELECT "$$users0"."id" FROM "users" AS "$$users0" LIMIT ({guard_sql})))"#
        ),
    );
    assert_eq!(params, guard_params);
}

#[test]
fn single_ref_chain_merges() {
    let (sql, params) = sql("users.manager.manager");
    assert_sql_eq(
        &sql,
        r#"SELECT "$$users2".* FROM "users" AS "$$users2" WHERE ("$$users2"."id" IN (SELECT "$$users1"."manager_id" FROM "users" AS "$$users1" WHERE ("$$users1"."id" IN (SELECT "$$users0"."manager_id" FROM "users" AS "$$users0"))))"#,
    );
    assert!(params.is_empty());
}

#[test]
fn collapsed_field_pushes_inside() {
    let (sql, params) = sql("users.first()?.age");
    assert_sql_eq(
        &sql,
        r#"SELECT "$$users1"."age" AS "$$val0" FROM "users" AS "$$users1" LIMIT ?"#,
    );
    assert_eq!(params, vec![int_param(1)]);
}

#[test]
fn collapsed_is_some_exists() {
    let (sql, params) = sql("users.first()?.manager.is_some()");
    assert_sql_eq(
        &sql,
        r#"SELECT EXISTS (SELECT "$$users1".* FROM "users" AS "$$users1" WHERE ("$$users1"."id" IN (SELECT "$$users0"."manager_id" FROM "users" AS "$$users0" LIMIT ?))) AS "$$val0""#,
    );
    assert_eq!(params, vec![int_param(1)]);
}

#[test]
fn list_collapse_is_some_exists() {
    let (sql, params) = sql("users { name, h = orders.first().is_some() }");
    assert_sql_eq(
        &sql,
        r#"SELECT "$$users0"."name", EXISTS (SELECT "$$orders1".* FROM "orders" AS "$$orders1" WHERE ("$$orders1"."user_id" = "$$users0"."id") LIMIT ?) AS "h" FROM "users" AS "$$users0""#,
    );
    assert_eq!(params, vec![int_param(1)]);
}

#[test]
fn list_collapse_is_none_exists() {
    let (sql, params) = sql("users { name, h = orders.first().is_none() }");
    assert_sql_eq(
        &sql,
        r#"SELECT "$$users0"."name", NOT EXISTS (SELECT "$$orders1".* FROM "orders" AS "$$orders1" WHERE ("$$orders1"."user_id" = "$$users0"."id") LIMIT ?) AS "h" FROM "users" AS "$$users0""#,
    );
    assert_eq!(params, vec![int_param(1)]);
}

#[test]
fn take_prefixed_collapse_is_some_exists() {
    let (sql, params) = sql("users.orders.take(1).first().is_some()");
    assert_sql_eq(
        &sql,
        r#"SELECT EXISTS (SELECT "$$orders2".* FROM "orders" AS "$$orders2" WHERE ("$$orders2"."user_id" IN (SELECT "$$users1"."id" FROM "users" AS "$$users1" LIMIT "min"(?, CASE WHEN ? >= ? THEN ? ELSE "$$panic"(?) END)))) AS "$$val0""#,
    );
    assert_eq!(
        params,
        vec![
            int_param(1),
            int_param(1),
            int_param(0),
            int_param(1),
            str_param("take requires a non-negative integer"),
        ]
    );
}

#[test]
fn optchain_is_some_split_merged() {
    let (sql, params) = sql("users { name, h = orders.first()?.items.is_some() }");
    assert_sql_eq(
        &sql,
        r#"SELECT "$$users0"."name", EXISTS (SELECT "$$items2".* FROM "items" AS "$$items2" WHERE ("$$items2"."order_id" IN (SELECT "$$orders1"."id" FROM "orders" AS "$$orders1" WHERE ("$$orders1"."user_id" = "$$users0"."id") LIMIT ?))) AS "h" FROM "users" AS "$$users0""#,
    );
    assert_eq!(params, vec![int_param(1)]);
}

#[test]
fn optchain_nested_collapse_presence() {
    let (sql, params) = sql("users { o = subordinates.orders.first()?.items.is_some() }");
    assert_sql_eq(
        &sql,
        r#"SELECT EXISTS (SELECT "$$users1".* FROM "users" AS "$$users1" WHERE ("$$users1"."manager_id" = "$$users0"."id")) AS "o" FROM "users" AS "$$users0""#,
    );
    assert!(params.is_empty());
}

#[test]
fn root_first_is_single() {
    use crate::codegen::model::{Cardinality, ChildRef, ResultShape};
    let compiled = compile("users.first()");
    assert_eq!(compiled.statements.len(), 1);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"SELECT "$$users0".* FROM "users" AS "$$users0" LIMIT ?"#,
    );
    assert_eq!(compiled.statements[0].params, vec![int_param(1)]);
    assert_eq!(
        compiled.result,
        ResultShape::Single {
            element: Box::new(ResultShape::Record(vec![
                ("name".to_string(), ResultShape::Column("name".to_string())),
                (
                    "address".to_string(),
                    ResultShape::Column("address".to_string())
                ),
                ("age".to_string(), ResultShape::Column("age".to_string())),
                (
                    "score".to_string(),
                    ResultShape::Column("score".to_string())
                ),
                (
                    "balance".to_string(),
                    ResultShape::Column("balance".to_string())
                ),
                (
                    "active".to_string(),
                    ResultShape::Column("active".to_string())
                ),
                (
                    "nickname".to_string(),
                    ResultShape::Column("nickname".to_string())
                ),
                ("lat".to_string(), ResultShape::Column("lat".to_string())),
                ("lng".to_string(), ResultShape::Column("lng".to_string())),
                ("t0".to_string(), ResultShape::Column("t0".to_string())),
                ("t1".to_string(), ResultShape::Column("t1".to_string())),
                ("t2".to_string(), ResultShape::Column("t2".to_string())),
                (
                    "created".to_string(),
                    ResultShape::Column("created".to_string())
                ),
                ("ttl".to_string(), ResultShape::Column("ttl".to_string())),
            ])),
            child: ChildRef {
                stmt: 0,
                parent: None,
                cardinality: Cardinality::One,
                tag: None,
            },
        }
    );
}

#[test]
fn root_first_over_nested_literal_is_single() {
    use crate::codegen::model::{Cardinality, ChildRef, ParentRef, ResultShape};
    let compiled = compile("[[1], [2]].first()");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"SELECT "lit0"."c0" FROM "$$lit_1_I"('lit0') AS "lit0" LIMIT ?"#,
    );
    assert_eq!(compiled.statements[0].params, vec![int_param(1)]);
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "lit1".* FROM "$$lit_2_II"('lit1') AS "lit1" WHERE ("lit1"."c0" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"()))"#,
    );
    assert!(compiled.statements[1].params.is_empty());
    assert_eq!(
        compiled.result,
        ResultShape::Single {
            element: Box::new(ResultShape::List {
                element: Box::new(ResultShape::Column("c1".to_string())),
                child: ChildRef {
                    stmt: 1,
                    parent: Some(ParentRef {
                        stmt: 0,
                        parent_key: "c0".to_string(),
                        child_key: "c0".to_string(),
                        negated: false,
                    }),
                    cardinality: Cardinality::Many,
                    tag: None,
                },
            }),
            child: ChildRef {
                stmt: 0,
                parent: None,
                cardinality: Cardinality::One,
                tag: None,
            },
        }
    );
}

#[test]
fn root_first_over_nested_navigation_is_single() {
    use crate::codegen::model::{Cardinality, ChildRef, ParentRef, ResultShape};
    let compiled = compile("users.orders.first()");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"SELECT "$$users0"."id" FROM "users" AS "$$users0" LIMIT ?"#,
    );
    assert_eq!(compiled.statements[0].params, vec![int_param(1)]);
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$orders1".* FROM "orders" AS "$$orders1" WHERE ("$$orders1"."user_id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"()))"#,
    );
    assert!(compiled.statements[1].params.is_empty());
    assert_eq!(
        compiled.result,
        ResultShape::Single {
            element: Box::new(ResultShape::List {
                element: Box::new(ResultShape::Record(vec![(
                    "total".to_string(),
                    ResultShape::Column("total".to_string()),
                )])),
                child: ChildRef {
                    stmt: 1,
                    parent: Some(ParentRef {
                        stmt: 0,
                        parent_key: "id".to_string(),
                        child_key: "user_id".to_string(),
                        negated: false,
                    }),
                    cardinality: Cardinality::Many,
                    tag: None,
                },
            }),
            child: ChildRef {
                stmt: 0,
                parent: None,
                cardinality: Cardinality::One,
                tag: None,
            },
        }
    );
}

#[test]
fn optional_hop_some_equality() {
    let (sql, params) = sql(r#"users[mentor?.name == some("Ada")] { name }"#);
    assert_sql_eq(
        &sql,
        r#"SELECT "$$users0"."name" FROM "users" AS "$$users0" WHERE ((SELECT "$$users1"."name" FROM "users" AS "$$users1" WHERE ("$$users1"."id" = "$$users0"."mentor_id")) IS ?)"#,
    );
    assert_eq!(
        params,
        vec![Param::Value(ParamValue::String("Ada".to_string()))]
    );
}

#[test]
fn optional_hop_projection() {
    let (sql, params) = sql("users { mname = mentor?.name }");
    assert_sql_eq(
        &sql,
        r#"SELECT (SELECT "$$users1"."name" FROM "users" AS "$$users1" WHERE ("$$users1"."id" = "$$users0"."mentor_id")) AS "mname" FROM "users" AS "$$users0""#,
    );
    assert!(params.is_empty());
}

#[test]
fn bare_nested_splits() {
    use crate::codegen::model::{Cardinality, ChildRef, ParentRef, ResultShape};
    let compiled = compile("users.orders");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"SELECT "$$users0"."id" FROM "users" AS "$$users0""#,
    );
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$orders1".* FROM "orders" AS "$$orders1" WHERE ("$$orders1"."user_id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"()))"#,
    );
    assert!(compiled.statements[1].params.is_empty());
    assert_eq!(
        compiled.result,
        ResultShape::List {
            element: Box::new(ResultShape::List {
                element: Box::new(ResultShape::Record(vec![(
                    "total".to_string(),
                    ResultShape::Column("total".to_string())
                ),])),
                child: ChildRef {
                    stmt: 1,
                    parent: Some(ParentRef {
                        stmt: 0,
                        parent_key: "id".to_string(),
                        child_key: "user_id".to_string(),
                        negated: false,
                    }),
                    cardinality: Cardinality::Many,
                    tag: None,
                },
            }),
            child: ChildRef {
                stmt: 1,
                parent: Some(ParentRef {
                    stmt: 0,
                    parent_key: "id".to_string(),
                    child_key: "user_id".to_string(),
                    negated: false,
                }),
                cardinality: Cardinality::Many,
                tag: None,
            },
        }
    );
}

#[test]
fn split_parent_carries_limit() {
    let (guard_sql, guard_params) = case_pos_guard(2, "take");
    let compiled = compile("users.take(2).orders");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[0].sql,
        &format!(r#"SELECT "$$users0"."id" FROM "users" AS "$$users0" LIMIT ({guard_sql})"#),
    );
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$orders1".* FROM "orders" AS "$$orders1" WHERE ("$$orders1"."user_id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"()))"#,
    );
    assert_eq!(compiled.statements[0].params, guard_params);
}

#[test]
fn split_merged_parent() {
    let compiled = compile("users.orders.flatten().items");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"SELECT "$$orders1"."id" FROM "orders" AS "$$orders1" WHERE ("$$orders1"."user_id" IN (SELECT "$$users0"."id" FROM "users" AS "$$users0"))"#,
    );
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$items2".* FROM "items" AS "$$items2" WHERE ("$$items2"."order_id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"()))"#,
    );
}

#[test]
fn nested_projection_items() {
    use crate::codegen::model::{Cardinality, ChildRef, ParentRef, ResultShape};
    let compiled = compile("users { name, o = orders }");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"SELECT "$$users0"."name", "$$users0"."id" FROM "users" AS "$$users0""#,
    );
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$orders1".* FROM "orders" AS "$$orders1" WHERE ("$$orders1"."user_id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"()))"#,
    );
    assert_eq!(
        compiled.result,
        ResultShape::List {
            element: Box::new(ResultShape::Record(vec![
                ("name".to_string(), ResultShape::Column("name".to_string())),
                (
                    "o".to_string(),
                    ResultShape::List {
                        element: Box::new(ResultShape::Record(vec![(
                            "total".to_string(),
                            ResultShape::Column("total".to_string())
                        ),])),
                        child: ChildRef {
                            stmt: 1,
                            parent: Some(ParentRef {
                                stmt: 0,
                                parent_key: "id".to_string(),
                                child_key: "user_id".to_string(),
                                negated: false,
                            }),
                            cardinality: Cardinality::Many,
                            tag: None,
                        },
                    }
                ),
            ])),
            child: ChildRef {
                stmt: 0,
                parent: None,
                cardinality: Cardinality::Many,
                tag: None,
            },
        }
    );
}

#[test]
fn projected_item_pipeline() {
    let compiled = compile("users { name, o = orders.filter(total > 1.0) { total } }");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"SELECT "$$users0"."name", "$$users0"."id" FROM "users" AS "$$users0""#,
    );
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$orders1"."total", "$$orders1"."user_id" FROM "orders" AS "$$orders1" WHERE ("$$orders1"."total" > ?) AND ("$$orders1"."user_id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"()))"#,
    );
    assert_eq!(
        compiled.statements[1].params,
        vec![Param::Value(ParamValue::Dec(rust_decimal::Decimal::new(
            10, 1
        )))]
    );
}

#[test]
fn per_parent_single() {
    use crate::codegen::model::{Cardinality, ChildRef, ParentRef, ResultShape};
    let compiled = compile("users { name, f = orders.first() }");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"SELECT "$$users0"."name", "$$users0"."id" FROM "users" AS "$$users0""#,
    );
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$orders1".* FROM "orders" AS "$$orders1" WHERE ("$$orders1"."user_id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"()))"#,
    );
    assert_eq!(
        compiled.result,
        ResultShape::List {
            element: Box::new(ResultShape::Record(vec![
                ("name".to_string(), ResultShape::Column("name".to_string())),
                (
                    "f".to_string(),
                    ResultShape::Single {
                        element: Box::new(ResultShape::Record(vec![(
                            "total".to_string(),
                            ResultShape::Column("total".to_string())
                        ),])),
                        child: ChildRef {
                            stmt: 1,
                            parent: Some(ParentRef {
                                stmt: 0,
                                parent_key: "id".to_string(),
                                child_key: "user_id".to_string(),
                                negated: false,
                            }),
                            cardinality: Cardinality::One,
                            tag: None,
                        },
                    }
                ),
            ])),
            child: ChildRef {
                stmt: 0,
                parent: None,
                cardinality: Cardinality::Many,
                tag: None,
            },
        }
    );
}

#[test]
fn broadcast_item() {
    let compiled = compile("users { name, c = configs }");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"SELECT "$$users0"."name" FROM "users" AS "$$users0""#,
    );
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$configs1".* FROM "configs" AS "$$configs1""#,
    );
}

#[test]
fn global_single_inlines() {
    let (sql, params) = sql("users { t = configs.first()?.name }");
    assert_sql_eq(
        &sql,
        r#"SELECT (SELECT "$$configs1"."name" FROM "configs" AS "$$configs1" LIMIT ?) AS "t" FROM "users" AS "$$users0""#,
    );
    assert_eq!(params, vec![int_param(1)]);
}

#[test]
fn ordered_collapse() {
    let compiled = compile("users { f = orders.sort(total).first() }");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$subq2".* FROM (SELECT "$$orders1".*, "ROW_NUMBER"() OVER (PARTITION BY "$$orders1"."user_id" ORDER BY "$$orders1"."total" ASC) AS "$$rn" FROM "orders" AS "$$orders1" WHERE ("$$orders1"."user_id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"()))) AS "$$subq2" WHERE ("$$subq2"."$$rn" <= ?)"#,
    );
    assert_eq!(compiled.statements[1].params, vec![int_param(1)]);
}

#[test]
fn nth_collapse() {
    let compiled = compile("users { x = orders.nth(0) }");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$subq2".* FROM (SELECT "$$orders1".*, "ROW_NUMBER"() OVER (PARTITION BY "$$orders1"."user_id") AS "$$rn" FROM "orders" AS "$$orders1" WHERE ("$$orders1"."user_id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"()))) AS "$$subq2" WHERE ("$$subq2"."$$rn" = ?)"#,
    );
    assert_eq!(compiled.statements[1].params, vec![int_param(1)]);
}

#[test]
fn take_prefixed_collapse_literal_bound() {
    let compiled = compile("users { f = orders.take(2).first() }");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$subq2".* FROM (SELECT "$$orders1".*, "ROW_NUMBER"() OVER (PARTITION BY "$$orders1"."user_id") AS "$$rn" FROM "orders" AS "$$orders1" WHERE ("$$orders1"."user_id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"()))) AS "$$subq2" WHERE ("$$subq2"."$$rn" <= (CASE WHEN ? >= ? THEN ? ELSE "$$panic"(?) END)) AND ("$$subq2"."$$rn" <= ?)"#,
    );
    assert_eq!(
        compiled.statements[1].params,
        vec![
            int_param(2),
            int_param(0),
            int_param(2),
            str_param("take requires a non-negative integer"),
            int_param(1),
        ]
    );
}

#[test]
fn nested_windowed_collapse() {
    let compiled = compile("users { o = orders { f = items.sort(sku).first() } }");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$subq3".* FROM (SELECT "$$items2".*, "ROW_NUMBER"() OVER (PARTITION BY "$$items2"."order_id" ORDER BY "$$items2"."sku" ASC) AS "$$rn" FROM "items" AS "$$items2" WHERE ("$$items2"."order_id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"()))) AS "$$subq3" WHERE ("$$subq3"."$$rn" <= ?)"#,
    );
    assert_eq!(compiled.statements[2].params, vec![int_param(1)]);
}

#[test]
fn skip_prefixed_collapse() {
    let compiled = compile("users { o = orders.skip(1).first() }");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$subq2".* FROM (SELECT "$$orders1".*, "ROW_NUMBER"() OVER (PARTITION BY "$$orders1"."user_id") AS "$$rn" FROM "orders" AS "$$orders1" WHERE ("$$orders1"."user_id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"()))) AS "$$subq2" WHERE ("$$subq2"."$$rn" > (CASE WHEN ? >= ? THEN ? ELSE "$$panic"(?) END)) AND ("$$subq2"."$$rn" <= "$$add"(CASE WHEN ? >= ? THEN ? ELSE "$$panic"(?) END, ?))"#,
    );
    assert_eq!(
        compiled.statements[1].params,
        vec![
            int_param(1),
            int_param(0),
            int_param(1),
            str_param("skip requires a non-negative integer"),
            int_param(1),
            int_param(0),
            int_param(1),
            str_param("skip requires a non-negative integer"),
            int_param(1),
        ]
    );
}

#[test]
fn sort_nth_collapse() {
    let compiled = compile("users { x = orders.sort(total).nth(1) }");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$subq2".* FROM (SELECT "$$orders1".*, "ROW_NUMBER"() OVER (PARTITION BY "$$orders1"."user_id" ORDER BY "$$orders1"."total" ASC) AS "$$rn" FROM "orders" AS "$$orders1" WHERE ("$$orders1"."user_id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"()))) AS "$$subq2" WHERE ("$$subq2"."$$rn" = ?)"#,
    );
    assert_eq!(compiled.statements[1].params, vec![int_param(2)]);
}

#[test]
fn nth_expr_collapse() {
    let compiled = compile("users { x = orders.nth(1 + 1) }");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$subq2".* FROM (SELECT "$$orders1".*, "ROW_NUMBER"() OVER (PARTITION BY "$$orders1"."user_id") AS "$$rn" FROM "orders" AS "$$orders1" WHERE ("$$orders1"."user_id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"()))) AS "$$subq2" WHERE ("$$subq2"."$$rn" = "$$add"(CASE WHEN "$$add"(?, ?) >= ? THEN "$$add"(?, ?) ELSE "$$panic"(?) END, ?))"#,
    );
    assert_eq!(
        compiled.statements[1].params,
        vec![
            int_param(1),
            int_param(1),
            int_param(0),
            int_param(1),
            int_param(1),
            str_param("nth requires a non-negative integer"),
            int_param(1),
        ]
    );
}

#[test]
fn literal_first_collapse() {
    let compiled = compile("users { x = [{a = 1}, {a = 2}].first() }");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "lit1"."c0" AS "a" FROM "$$lit_1_I"('lit1') AS "lit1" LIMIT ?"#,
    );
    assert_eq!(compiled.statements[1].params, vec![int_param(1)]);
}

#[test]
fn literal_first_field_nav() {
    let (sql, params) = sql("users { x = [{a = 1}, {a = 2}].first()?.a }");
    assert_sql_eq(
        &sql,
        r#"SELECT (SELECT "lit1"."c0" FROM "$$lit_1_I"('lit1') AS "lit1" LIMIT ?) AS "x" FROM "users" AS "$$users0""#,
    );
    assert_eq!(params, vec![int_param(1)]);
}

#[test]
fn unkeyed_ordered_collapse() {
    let compiled = compile("users { x = configs.sort(name).first() }");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$configs1".* FROM "configs" AS "$$configs1" ORDER BY "$$configs1"."name" ASC LIMIT ?"#,
    );
    assert_eq!(compiled.statements[1].params, vec![int_param(1)]);
}

#[test]
fn take_prefixed_row_collapse() {
    use crate::codegen::model::{Cardinality, ChildRef, ParentRef, ResultShape};
    let compiled = compile("users { o = orders.take(age).first() }");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"CREATE TEMP TABLE "$$split2" AS SELECT "$$users0".*, "$$users0"."id" AS "$$key" FROM "users" AS "$$users0""#,
    );
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$split2"."id" FROM "$$split2" AS "$$split2""#,
    );
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$subq4".* FROM (SELECT "$$orders3".*, "ROW_NUMBER"() OVER (PARTITION BY "$$orders3"."user_id") AS "$$rn" FROM "orders" AS "$$orders3" WHERE ("$$orders3"."user_id" IN (SELECT "$$split2"."$$key" FROM "$$split2" AS "$$split2"))) AS "$$subq4" WHERE ("$$subq4"."$$rn" <= (CASE WHEN (SELECT "$$split2"."age" FROM "$$split2" AS "$$split2" WHERE ("$$split2"."$$key" = "$$subq4"."user_id")) >= ? THEN (SELECT "$$split2"."age" FROM "$$split2" AS "$$split2" WHERE ("$$split2"."$$key" = "$$subq4"."user_id")) ELSE "$$panic"(?) END)) AND ("$$subq4"."$$rn" <= ?)"#,
    );
    assert_eq!(
        compiled.result,
        ResultShape::List {
            element: Box::new(ResultShape::Record(vec![(
                "o".to_string(),
                ResultShape::Single {
                    element: Box::new(ResultShape::Record(vec![(
                        "total".to_string(),
                        ResultShape::Column("total".to_string()),
                    )])),
                    child: ChildRef {
                        stmt: 2,
                        parent: Some(ParentRef {
                            stmt: 1,
                            parent_key: "id".to_string(),
                            child_key: "user_id".to_string(),
                            negated: false,
                        }),
                        cardinality: Cardinality::One,
                        tag: None,
                    },
                },
            )])),
            child: ChildRef {
                stmt: 1,
                parent: None,
                cardinality: Cardinality::Many,
                tag: None,
            },
        }
    );
}

#[test]
fn take_prefixed_row_collapse_scope() {
    let compiled = compile("users { o = orders.take(age).nth(age - 29) }");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$subq4".* FROM (SELECT "$$orders3".*, "ROW_NUMBER"() OVER (PARTITION BY "$$orders3"."user_id") AS "$$rn" FROM "orders" AS "$$orders3" WHERE ("$$orders3"."user_id" IN (SELECT "$$split2"."$$key" FROM "$$split2" AS "$$split2"))) AS "$$subq4" WHERE ("$$subq4"."$$rn" <= (CASE WHEN (SELECT "$$split2"."age" FROM "$$split2" AS "$$split2" WHERE ("$$split2"."$$key" = "$$subq4"."user_id")) >= ? THEN (SELECT "$$split2"."age" FROM "$$split2" AS "$$split2" WHERE ("$$split2"."$$key" = "$$subq4"."user_id")) ELSE "$$panic"(?) END)) AND ("$$subq4"."$$rn" = "$$add"(CASE WHEN "$$sub"((SELECT "$$split2"."age" FROM "$$split2" AS "$$split2" WHERE ("$$split2"."$$key" = "$$subq4"."user_id")), ?) >= ? THEN "$$sub"((SELECT "$$split2"."age" FROM "$$split2" AS "$$split2" WHERE ("$$split2"."$$key" = "$$subq4"."user_id")), ?) ELSE "$$panic"(?) END, ?))"#,
    );
    let compiled = compile("users { o = orders.take(age).skip(1).first() }");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$subq4".* FROM (SELECT "$$orders3".*, "ROW_NUMBER"() OVER (PARTITION BY "$$orders3"."user_id") AS "$$rn" FROM "orders" AS "$$orders3" WHERE ("$$orders3"."user_id" IN (SELECT "$$split2"."$$key" FROM "$$split2" AS "$$split2"))) AS "$$subq4" WHERE ("$$subq4"."$$rn" <= (CASE WHEN (SELECT "$$split2"."age" FROM "$$split2" AS "$$split2" WHERE ("$$split2"."$$key" = "$$subq4"."user_id")) >= ? THEN (SELECT "$$split2"."age" FROM "$$split2" AS "$$split2" WHERE ("$$split2"."$$key" = "$$subq4"."user_id")) ELSE "$$panic"(?) END)) AND ("$$subq4"."$$rn" > (CASE WHEN ? >= ? THEN ? ELSE "$$panic"(?) END)) AND ("$$subq4"."$$rn" <= "$$add"(CASE WHEN ? >= ? THEN ? ELSE "$$panic"(?) END, ?))"#,
    );
    let compiled = compile("users { o = orders.sort(age).first() }");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$subq4".* FROM (SELECT "$$orders3".*, "ROW_NUMBER"() OVER (PARTITION BY "$$orders3"."user_id") AS "$$rn" FROM "orders" AS "$$orders3" WHERE ("$$orders3"."user_id" IN (SELECT "$$split2"."$$key" FROM "$$split2" AS "$$split2")) ORDER BY (SELECT "$$split2"."age" FROM "$$split2" AS "$$split2" WHERE ("$$split2"."$$key" = "$$orders3"."user_id")) ASC) AS "$$subq4" WHERE ("$$subq4"."$$rn" <= ?)"#,
    );
}

#[test]
fn union_collapse() {
    use crate::codegen::model::{Cardinality, ChildRef, ParentRef, ResultShape};
    let compiled = compile("users { x = (if active { configs } else { configs }).first() }");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"CREATE TEMP TABLE "$$split1" AS SELECT "$$users0".*, "ROW_NUMBER"() OVER () AS "$$key" FROM "users" AS "$$users0""#,
    );
    assert!(compiled.statements[0].params.is_empty());
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$subq5".* FROM (SELECT "$$subq4".*, "ROW_NUMBER"() OVER (PARTITION BY "$$subq4"."$$pkey") AS "$$rn" FROM (SELECT "$$configs2"."name", "$$configs2"."owner", "$$split1"."$$key" AS "$$pkey" FROM "$$split1" AS "$$split1", "configs" AS "$$configs2" WHERE "$$split1"."active" UNION ALL SELECT "$$configs3"."name", "$$configs3"."owner", "$$split1"."$$key" AS "$$pkey" FROM "$$split1" AS "$$split1", "configs" AS "$$configs3" WHERE NOT "$$split1"."active") AS "$$subq4") AS "$$subq5" WHERE ("$$subq5"."$$rn" <= ?)"#,
    );
    assert_eq!(compiled.statements[2].params, vec![int_param(1)]);
    assert_eq!(
        compiled.result,
        ResultShape::List {
            element: Box::new(ResultShape::Record(vec![(
                "x".to_string(),
                ResultShape::Single {
                    element: Box::new(ResultShape::Record(vec![
                        ("name".to_string(), ResultShape::Column("name".to_string()),),
                        (
                            "owner".to_string(),
                            ResultShape::Column("owner".to_string()),
                        ),
                    ])),
                    child: ChildRef {
                        stmt: 2,
                        parent: Some(ParentRef {
                            stmt: 0,
                            parent_key: "$$key".to_string(),
                            child_key: "$$pkey".to_string(),
                            negated: false,
                        }),
                        cardinality: Cardinality::One,
                        tag: None,
                    },
                },
            )])),
            child: ChildRef {
                stmt: 1,
                parent: None,
                cardinality: Cardinality::Many,
                tag: None,
            },
        }
    );
}

#[test]
fn union_collapse_nth() {
    use crate::codegen::model::{Cardinality, ChildRef, ParentRef, ResultShape};
    let compiled = compile("users { x = (if active { configs } else { configs }).nth(1) }");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$subq5".* FROM (SELECT "$$subq4".*, "ROW_NUMBER"() OVER (PARTITION BY "$$subq4"."$$pkey") AS "$$rn" FROM (SELECT "$$configs2"."name", "$$configs2"."owner", "$$split1"."$$key" AS "$$pkey" FROM "$$split1" AS "$$split1", "configs" AS "$$configs2" WHERE "$$split1"."active" UNION ALL SELECT "$$configs3"."name", "$$configs3"."owner", "$$split1"."$$key" AS "$$pkey" FROM "$$split1" AS "$$split1", "configs" AS "$$configs3" WHERE NOT "$$split1"."active") AS "$$subq4") AS "$$subq5" WHERE ("$$subq5"."$$rn" = ?)"#,
    );
    assert_eq!(compiled.statements[2].params, vec![int_param(2)]);
    let single = ResultShape::Single {
        element: Box::new(ResultShape::Record(vec![
            ("name".to_string(), ResultShape::Column("name".to_string())),
            (
                "owner".to_string(),
                ResultShape::Column("owner".to_string()),
            ),
        ])),
        child: ChildRef {
            stmt: 2,
            parent: Some(ParentRef {
                stmt: 0,
                parent_key: "$$key".to_string(),
                child_key: "$$pkey".to_string(),
                negated: false,
            }),
            cardinality: Cardinality::One,
            tag: None,
        },
    };
    assert_eq!(
        compiled.result,
        ResultShape::List {
            element: Box::new(ResultShape::Record(vec![("x".to_string(), single,)])),
            child: ChildRef {
                stmt: 1,
                parent: None,
                cardinality: Cardinality::Many,
                tag: None,
            },
        }
    );
}

#[test]
fn union_cross_middle() {
    use crate::codegen::model::{Cardinality, ChildRef, ParentRef, ResultShape};
    let compiled = compile("users { x = if active { configs } else { configs } }");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"CREATE TEMP TABLE "$$split1" AS SELECT "$$users0".*, "ROW_NUMBER"() OVER () AS "$$key" FROM "users" AS "$$users0""#,
    );
    assert!(compiled.statements[0].params.is_empty());
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$configs2"."name", "$$configs2"."owner", "$$split1"."$$key" AS "$$pkey" FROM "$$split1" AS "$$split1", "configs" AS "$$configs2" WHERE "$$split1"."active" UNION ALL SELECT "$$configs3"."name", "$$configs3"."owner", "$$split1"."$$key" AS "$$pkey" FROM "$$split1" AS "$$split1", "configs" AS "$$configs3" WHERE NOT "$$split1"."active""#,
    );
    assert!(compiled.statements[2].params.is_empty());
    let middle = ResultShape::List {
        element: Box::new(ResultShape::Record(vec![
            ("name".to_string(), ResultShape::Column("name".to_string())),
            (
                "owner".to_string(),
                ResultShape::Column("owner".to_string()),
            ),
        ])),
        child: ChildRef {
            stmt: 2,
            parent: Some(ParentRef {
                stmt: 0,
                parent_key: "$$key".to_string(),
                child_key: "$$pkey".to_string(),
                negated: false,
            }),
            cardinality: Cardinality::Many,
            tag: None,
        },
    };
    assert_eq!(
        compiled.result,
        ResultShape::List {
            element: Box::new(ResultShape::Record(vec![("x".to_string(), middle,)])),
            child: ChildRef {
                stmt: 1,
                parent: None,
                cardinality: Cardinality::Many,
                tag: None,
            },
        }
    );
}

#[test]
fn sibling_children_reverse() {
    use crate::codegen::model::{Cardinality, ChildRef, ParentRef, ResultShape};
    let compiled = compile("users { o = orders, f = orders.first() }");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"SELECT "$$users0"."id" FROM "users" AS "$$users0""#,
    );
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$orders2".* FROM "orders" AS "$$orders2" WHERE ("$$orders2"."user_id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"()))"#,
    );
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$orders1".* FROM "orders" AS "$$orders1" WHERE ("$$orders1"."user_id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"()))"#,
    );
    assert_eq!(
        compiled.result,
        ResultShape::List {
            element: Box::new(ResultShape::Record(vec![
                (
                    "o".to_string(),
                    ResultShape::List {
                        element: Box::new(ResultShape::Record(vec![(
                            "total".to_string(),
                            ResultShape::Column("total".to_string())
                        ),])),
                        child: ChildRef {
                            stmt: 2,
                            parent: Some(ParentRef {
                                stmt: 0,
                                parent_key: "id".to_string(),
                                child_key: "user_id".to_string(),
                                negated: false,
                            }),
                            cardinality: Cardinality::Many,
                            tag: None,
                        },
                    }
                ),
                (
                    "f".to_string(),
                    ResultShape::Single {
                        element: Box::new(ResultShape::Record(vec![(
                            "total".to_string(),
                            ResultShape::Column("total".to_string())
                        ),])),
                        child: ChildRef {
                            stmt: 1,
                            parent: Some(ParentRef {
                                stmt: 0,
                                parent_key: "id".to_string(),
                                child_key: "user_id".to_string(),
                                negated: false,
                            }),
                            cardinality: Cardinality::One,
                            tag: None,
                        },
                    }
                ),
            ])),
            child: ChildRef {
                stmt: 0,
                parent: None,
                cardinality: Cardinality::Many,
                tag: None,
            },
        }
    );
}

#[test]
fn temp_correlated_filter() {
    use crate::codegen::model::{Cardinality, ChildRef, ParentRef, ResultShape, StatementKind};
    let compiled = compile("users { o = orders.filter(total > balance) }");
    assert_eq!(compiled.statements.len(), 3);
    assert!(matches!(
        compiled.statements[0].kind,
        StatementKind::Temp { .. }
    ));
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"CREATE TEMP TABLE "$$split1" AS SELECT "$$users0".*, "$$users0"."id" AS "$$key" FROM "users" AS "$$users0""#,
    );
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$split1"."$$key" FROM "$$split1" AS "$$split1""#,
    );
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$orders2".* FROM "orders" AS "$$orders2" WHERE ("$$orders2"."total" > (SELECT "$$split1"."balance" FROM "$$split1" AS "$$split1" WHERE ("$$split1"."$$key" = "$$orders2"."user_id"))) AND ("$$orders2"."user_id" IN (SELECT "$$split1"."$$key" FROM "$$split1" AS "$$split1"))"#,
    );
    assert_eq!(
        compiled.result,
        ResultShape::List {
            element: Box::new(ResultShape::Record(vec![(
                "o".to_string(),
                ResultShape::List {
                    element: Box::new(ResultShape::Record(vec![(
                        "total".to_string(),
                        ResultShape::Column("total".to_string())
                    ),])),
                    child: ChildRef {
                        stmt: 2,
                        parent: Some(ParentRef {
                            stmt: 0,
                            parent_key: "$$key".to_string(),
                            child_key: "user_id".to_string(),
                            negated: false,
                        }),
                        cardinality: Cardinality::Many,
                        tag: None,
                    },
                }
            )])),
            child: ChildRef {
                stmt: 1,
                parent: None,
                cardinality: Cardinality::Many,
                tag: None,
            },
        }
    );
}

#[test]
fn per_parent_take_window() {
    let (guard_sql, guard_params) = case_pos_guard(2, "take");
    let compiled = compile("users { o = orders.take(2) }");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[1].sql,
        &format!(
            r#"SELECT "$$subq2".* FROM (SELECT "$$orders1".*, "ROW_NUMBER"() OVER (PARTITION BY "$$orders1"."user_id") AS "$$rn" FROM "orders" AS "$$orders1" WHERE ("$$orders1"."user_id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"()))) AS "$$subq2" WHERE ("$$subq2"."$$rn" <= ({guard_sql}))"#
        ),
    );
    assert_eq!(compiled.statements[1].params, guard_params);
}

#[test]
fn per_parent_take_ordered_window() {
    let (guard_sql, guard_params) = case_pos_guard(2, "take");
    let compiled = compile("users { o = orders.sort(total).take(2) }");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[1].sql,
        &format!(
            r#"SELECT "$$subq2".* FROM (SELECT "$$orders1".*, "ROW_NUMBER"() OVER (PARTITION BY "$$orders1"."user_id" ORDER BY "$$orders1"."total" ASC) AS "$$rn" FROM "orders" AS "$$orders1" WHERE ("$$orders1"."user_id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"()))) AS "$$subq2" WHERE ("$$subq2"."$$rn" <= ({guard_sql}))"#
        ),
    );
    assert_eq!(compiled.statements[1].params, guard_params);
}

#[test]
fn temp_bound_window() {
    use crate::codegen::model::StatementKind;
    let compiled = compile("users { o = orders.take(age) }");
    assert_eq!(compiled.statements.len(), 3);
    assert!(matches!(
        compiled.statements[0].kind,
        StatementKind::Temp { .. }
    ));
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"CREATE TEMP TABLE "$$split1" AS SELECT "$$users0".*, "$$users0"."id" AS "$$key" FROM "users" AS "$$users0""#,
    );
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$split1"."$$key" FROM "$$split1" AS "$$split1""#,
    );
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$subq3".* FROM (SELECT "$$orders2".*, "ROW_NUMBER"() OVER (PARTITION BY "$$orders2"."user_id") AS "$$rn" FROM "orders" AS "$$orders2" WHERE ("$$orders2"."user_id" IN (SELECT "$$split1"."$$key" FROM "$$split1" AS "$$split1"))) AS "$$subq3" WHERE ("$$subq3"."$$rn" <= (CASE WHEN (SELECT "$$split1"."age" FROM "$$split1" AS "$$split1" WHERE ("$$split1"."$$key" = "$$subq3"."user_id")) >= ? THEN (SELECT "$$split1"."age" FROM "$$split1" AS "$$split1" WHERE ("$$split1"."$$key" = "$$subq3"."user_id")) ELSE "$$panic"(?) END))"#,
    );
    assert_eq!(
        compiled.statements[2].params,
        vec![
            int_param(0),
            Param::Value(ParamValue::String(
                "take requires a non-negative integer".to_string()
            )),
        ]
    );
}

#[test]
fn correlated_global_single_inlines() {
    let (sql, params) = sql("users { t = configs.filter(owner == email).first()?.name }");
    assert_sql_eq(
        &sql,
        r#"SELECT (SELECT "$$configs1"."name" FROM "configs" AS "$$configs1" WHERE ("$$configs1"."owner" = "$$users0"."address") LIMIT ?) AS "t" FROM "users" AS "$$users0""#,
    );
    assert_eq!(params, vec![int_param(1)]);
}

#[test]
fn array_contains_tailed() {
    let (sql_take, params_take) = sql(r#"users[tags.take(1).contains("x")] { name }"#);
    assert_sql_eq(
        &sql_take,
        r#"SELECT "$$users0"."name" FROM "users" AS "$$users0" WHERE EXISTS (SELECT "$$subq2".* FROM (SELECT "$$user_tags1"."tag_value" FROM "user_tags" AS "$$user_tags1" WHERE ("$$user_tags1"."user_key" = "$$users0"."id") LIMIT (CASE WHEN ? >= ? THEN ? ELSE "$$panic"(?) END)) AS "$$subq2" WHERE ("$$subq2"."tag_value" = ?))"#,
    );
    let (sql_skip, _) = sql(r#"users[tags.skip(1).contains("y")] { name }"#);
    assert_sql_eq(
        &sql_skip,
        r#"SELECT "$$users0"."name" FROM "users" AS "$$users0" WHERE EXISTS (SELECT "$$subq2".* FROM (SELECT "$$user_tags1"."tag_value" FROM "user_tags" AS "$$user_tags1" WHERE ("$$user_tags1"."user_key" = "$$users0"."id") LIMIT -1 OFFSET (CASE WHEN ? >= ? THEN ? ELSE "$$panic"(?) END)) AS "$$subq2" WHERE ("$$subq2"."tag_value" = ?))"#,
    );
    assert_eq!(
        params_take,
        vec![
            int_param(1),
            int_param(0),
            int_param(1),
            Param::Value(ParamValue::String(
                "take requires a non-negative integer".to_string()
            )),
            Param::Value(ParamValue::String("x".to_string())),
        ]
    );
}

#[test]
fn scalar_first_storage_tailed() {
    let (sql_first, _) = sql(r#"users { c = tags.take(1).first() }"#);
    assert_sql_eq(
        &sql_first,
        r#"SELECT (SELECT "$$user_tags1"."tag_value" AS "$$val2" FROM "user_tags" AS "$$user_tags1" WHERE ("$$user_tags1"."user_key" = "$$users0"."id") LIMIT "min"(?, CASE WHEN ? >= ? THEN ? ELSE "$$panic"(?) END)) AS "c" FROM "users" AS "$$users0""#,
    );
    let (sql_nth, params_nth) = sql(r#"users { c = scores.nth(1) }"#);
    assert_sql_eq(
        &sql_nth,
        r#"SELECT (SELECT "$$user_scores1"."score_value" AS "$$val2" FROM "user_scores" AS "$$user_scores1" WHERE ("$$user_scores1"."user_key" = "$$users0"."id") LIMIT ? OFFSET (CASE WHEN ? >= ? THEN ? ELSE "$$panic"(?) END)) AS "c" FROM "users" AS "$$users0""#,
    );
    assert_eq!(
        params_nth,
        vec![
            int_param(1),
            int_param(1),
            int_param(0),
            int_param(1),
            Param::Value(ParamValue::String(
                "nth requires a non-negative integer".to_string()
            )),
        ]
    );
}

#[test]
fn array_contains_exists() {
    let (sql, params) = sql(r#"users[tags.contains("vip")] { name }"#);
    assert_sql_eq(
        &sql,
        r#"SELECT "$$users0"."name" FROM "users" AS "$$users0" WHERE EXISTS (SELECT "$$user_tags1"."user_key" FROM "user_tags" AS "$$user_tags1" WHERE ("$$user_tags1"."user_key" = "$$users0"."id") AND ("$$user_tags1"."tag_value" = ?))"#,
    );
    assert_eq!(
        params,
        vec![Param::Value(ParamValue::String("vip".to_string()))]
    );
}

#[test]
fn cross_conjunct_pairing() {
    let compiled =
        compile("users { name, cs = configs.filter(owner == email && name == email) { name } }");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$configs1"."name", (SELECT "$$users2"."address" FROM "users" AS "$$users2" WHERE (("$$users2"."address" = "$$configs1"."owner") AND ("$$users2"."address" = "$$configs1"."name"))) AS "$$pkey" FROM "configs" AS "$$configs1""#,
    );
    assert!(compiled.statements[1].params.is_empty());
}

#[test]
fn cross_nav_multi_hop() {
    let compiled =
        compile("users { name, cs = configs.filter(owner == manager.manager.email) { name } }");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"CREATE TEMP TABLE "$$split2" AS SELECT "$$users0".*, (SELECT "$$users5"."address" AS "$$val6" FROM "users" AS "$$users5" WHERE ("$$users5"."id" = (SELECT "$$users3"."manager_id" AS "$$val4" FROM "users" AS "$$users3" WHERE ("$$users3"."id" = "$$users0"."manager_id")))) AS "$$nav1" FROM "users" AS "$$users0""#,
    );
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$configs7"."name", (SELECT "$$split2"."$$nav1" FROM "$$split2" AS "$$split2" WHERE ("$$split2"."$$nav1" = "$$configs7"."owner")) AS "$$pkey" FROM "configs" AS "$$configs7""#,
    );
}

#[test]
fn cross_nav_shared_carry() {
    let compiled = compile(
        r#"users { name, cs = configs.filter(manager.email == owner || manager.email == "x") { name } }"#,
    );
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_contains(&compiled.statements[0].sql, r#"AS "$$nav1" FROM "users""#);
    assert!(!compiled.statements[0].sql.contains("$$nav2"));
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$configs5"."name", (SELECT "$$split2"."$$nav1" FROM "$$split2" AS "$$split2" WHERE (("$$split2"."$$nav1" = "$$configs5"."owner") OR ("$$split2"."$$nav1" = ?))) AS "$$pkey" FROM "configs" AS "$$configs5""#,
    );
}

#[test]
fn cross_tuple_pairing() {
    let compiled =
        compile("users { name, cs = configs.filter((owner, owner) == (email, email)) { name } }");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$configs1"."name", (SELECT "$$users2"."address" FROM "users" AS "$$users2" WHERE ("$$users2"."address" = "$$configs1"."owner")) AS "$$pkey" FROM "configs" AS "$$configs1""#,
    );
    assert!(compiled.statements[1].params.is_empty());
    let compiled =
        compile(r#"users { name, cs = configs.filter((owner, name) == (email, "n1")) { name } }"#);
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$configs1"."name", (SELECT "$$users2"."address" FROM "users" AS "$$users2" WHERE ("$$users2"."address" = "$$configs1"."owner")) AS "$$pkey" FROM "configs" AS "$$configs1" WHERE ("$$configs1"."name" = ?)"#,
    );
    assert_eq!(
        compiled.statements[1].params,
        vec![Param::Value(ParamValue::String("n1".to_string()))]
    );
    let compiled =
        compile("users { name, cs = configs.filter((owner, name) == (email, email)) { name } }");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$configs1"."name", (SELECT "$$users2"."address" FROM "users" AS "$$users2" WHERE (("$$users2"."address" = "$$configs1"."owner") AND ("$$users2"."address" = "$$configs1"."name"))) AS "$$pkey" FROM "configs" AS "$$configs1""#,
    );
    assert!(compiled.statements[1].params.is_empty());
}

#[test]
fn cross_root_direct() {
    use crate::codegen::model::{Cardinality, ChildRef, ParentRef, ResultShape};
    let compiled = compile("users { name, cs = configs.filter(owner == email) }");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"SELECT "$$users0"."name", "$$users0"."address" FROM "users" AS "$$users0""#,
    );
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$configs1".*, (SELECT "$$users2"."address" FROM "users" AS "$$users2" WHERE ("$$users2"."address" = "$$configs1"."owner")) AS "$$pkey" FROM "configs" AS "$$configs1""#,
    );
    assert_eq!(
        compiled.result,
        ResultShape::List {
            element: Box::new(ResultShape::Record(vec![
                ("name".to_string(), ResultShape::Column("name".to_string())),
                (
                    "cs".to_string(),
                    ResultShape::List {
                        element: Box::new(ResultShape::Record(vec![
                            ("name".to_string(), ResultShape::Column("name".to_string())),
                            (
                                "owner".to_string(),
                                ResultShape::Column("owner".to_string())
                            ),
                        ])),
                        child: ChildRef {
                            stmt: 1,
                            parent: Some(ParentRef {
                                stmt: 0,
                                parent_key: "address".to_string(),
                                child_key: "$$pkey".to_string(),
                                negated: false,
                            }),
                            cardinality: Cardinality::Many,
                            tag: None,
                        },
                    }
                ),
            ])),
            child: ChildRef {
                stmt: 0,
                parent: None,
                cardinality: Cardinality::Many,
                tag: None,
            },
        }
    );
}

#[test]
fn cross_root_temp() {
    use crate::codegen::model::StatementKind;
    let compiled = compile("users[active] { name, cs = configs.filter(owner == email) }");
    assert_eq!(compiled.statements.len(), 3);
    assert!(matches!(
        compiled.statements[0].kind,
        StatementKind::Temp { .. }
    ));
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"CREATE TEMP TABLE "$$split1" AS SELECT "$$users0".* FROM "users" AS "$$users0" WHERE "$$users0"."active""#,
    );
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$split1"."name", "$$split1"."address" FROM "$$split1" AS "$$split1""#,
    );
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$configs2".*, (SELECT "$$split1"."address" FROM "$$split1" AS "$$split1" WHERE ("$$split1"."address" = "$$configs2"."owner")) AS "$$pkey" FROM "configs" AS "$$configs2""#,
    );
}

#[test]
fn cross_root_single() {
    let compiled = compile("users { c = configs.filter(owner == email).first() }");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$configs1".*, (SELECT "$$users2"."address" FROM "users" AS "$$users2" WHERE ("$$users2"."address" = "$$configs1"."owner")) AS "$$pkey" FROM "configs" AS "$$configs1""#,
    );
}

#[test]
fn collapse_pipeline_correlates() {
    let (sql, params) = sql("users { t = orders.filter(total > 1.0).first()?.total }");
    assert_sql_eq(
        &sql,
        r#"SELECT (SELECT "$$orders1"."total" FROM "orders" AS "$$orders1" WHERE ("$$orders1"."total" > ?) AND ("$$orders1"."user_id" = "$$users0"."id") LIMIT ?) AS "t" FROM "users" AS "$$users0""#,
    );
    assert_eq!(
        params,
        vec![
            Param::Value(ParamValue::Dec(rust_decimal::Decimal::new(10, 1))),
            int_param(1),
        ]
    );
}

#[test]
fn scalar_first_flat() {
    let (sql, params) = sql("users.name.first()");
    assert_sql_eq(
        &sql,
        r#"SELECT "$$users1"."name" AS "$$val0" FROM "users" AS "$$users1" LIMIT ?"#,
    );
    assert_eq!(params, vec![int_param(1)]);
}

#[test]
fn single_base_nav_chains() {
    let compiled = compile("users { t = manager.orders.first()?.total }");
    assert_eq!(compiled.statements.len(), 1);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"SELECT (SELECT (SELECT "$$orders2"."total" FROM "orders" AS "$$orders2" WHERE ("$$orders2"."user_id" = "$$users1"."id")) AS "$$val3" FROM "users" AS "$$users1" WHERE ("$$users1"."id" = "$$users0"."manager_id")) AS "t" FROM "users" AS "$$users0""#,
    );
}

#[test]
fn agg_len_bare() {
    let (sql, params) = sql("users.len()");
    assert_sql_eq(
        &sql,
        r#"SELECT COUNT(*) AS "$$val0" FROM "users" AS "$$users0""#,
    );
    assert!(params.is_empty());
}

#[test]
fn agg_len_per_parent() {
    let (sql, params) = sql("users { n = orders.len() }");
    assert_sql_eq(
        &sql,
        r#"SELECT (SELECT COUNT(*) AS "$$val9" FROM "orders" AS "$$orders1" WHERE ("$$orders1"."user_id" = "$$users0"."id")) AS "n" FROM "users" AS "$$users0""#,
    );
    assert!(params.is_empty());
}

#[test]
fn agg_len_filter() {
    let (sql, params) = sql("users[orders.len() > 2] { name }");
    assert_sql_eq(
        &sql,
        r#"SELECT "$$users0"."name" FROM "users" AS "$$users0" WHERE ((SELECT COUNT(*) AS "$$val9" FROM "orders" AS "$$orders1" WHERE ("$$orders1"."user_id" = "$$users0"."id")) > ?)"#,
    );
    assert_eq!(params, vec![int_param(2)]);
}

#[test]
fn agg_take_reduction_wraps() {
    {
        let (take_sql, take_params) = case_pos_guard(1, "take");
        let (sql1, params1) = sql("users { s = scores.take(1).sum() }");
        assert_sql_eq(
            &sql1,
            &format!(
                r#"SELECT (SELECT "SUM"("$$subq2"."score_value") AS "$$val3" FROM (SELECT "$$user_scores1".* FROM "user_scores" AS "$$user_scores1" WHERE ("$$user_scores1"."user_key" = "$$users0"."id") LIMIT ({take_sql})) AS "$$subq2") AS "s" FROM "users" AS "$$users0""#
            ),
        );
        assert_eq!(params1, take_params);
    }
    {
        let (sql1, _) = sql("users { s = scores.skip(1).sum() }");
        assert_sql_contains(&sql1, "OFFSET");
        assert_sql_contains(&sql1, "\"SUM\"");
    }
}

#[test]
fn agg_sum_over_dec() {
    let (sql, params) = sql("users { s = orders.sum_over(total) }");
    assert_sql_eq(
        &sql,
        r#"SELECT (SELECT "$$sum_dec"("$$orders1"."total") AS "$$val9" FROM "orders" AS "$$orders1" WHERE ("$$orders1"."user_id" = "$$users0"."id")) AS "s" FROM "users" AS "$$users0""#,
    );
    assert!(params.is_empty());
}

#[test]
fn agg_avg_over_dec() {
    let (sql, params) = sql("users { a = orders.avg_over(total) }");
    assert_sql_eq(
        &sql,
        r#"SELECT (SELECT "$$avg_dec"("$$orders1"."total") AS "$$val9" FROM "orders" AS "$$orders1" WHERE ("$$orders1"."user_id" = "$$users0"."id")) AS "a" FROM "users" AS "$$users0""#,
    );
    assert!(params.is_empty());
}

#[test]
fn agg_max_by_pushinside() {
    let (sql, params) = sql("users { m = orders.max_by(total)?.total }");
    assert_sql_eq(
        &sql,
        r#"SELECT (SELECT "$$orders1"."total" FROM "orders" AS "$$orders1" WHERE ("$$orders1"."user_id" = "$$users0"."id") ORDER BY "$$orders1"."total" DESC LIMIT ?) AS "m" FROM "users" AS "$$users0""#,
    );
    assert_eq!(params, vec![int_param(1)]);
}

#[test]
fn agg_sum_int_native() {
    let (sql, params) = sql("users { s = scores.sum() }");
    assert_sql_eq(
        &sql,
        r#"SELECT (SELECT "SUM"("$$user_scores1"."score_value") AS "$$val9" FROM "user_scores" AS "$$user_scores1" WHERE ("$$user_scores1"."user_key" = "$$users0"."id")) AS "s" FROM "users" AS "$$users0""#,
    );
    assert!(params.is_empty());
}

#[test]
fn agg_avg_int_float() {
    let (sql, params) = sql("users { a = scores.avg() }");
    assert_sql_eq(
        &sql,
        r#"SELECT (SELECT "AVG"("$$user_scores1"."score_value") AS "$$val9" FROM "user_scores" AS "$$user_scores1" WHERE ("$$user_scores1"."user_key" = "$$users0"."id")) AS "a" FROM "users" AS "$$users0""#,
    );
    assert!(params.is_empty());
}

#[test]
fn agg_sum_duration_custom() {
    let (sql, params) = sql("users { s = durs.sum() }");
    assert_sql_eq(
        &sql,
        r#"SELECT (SELECT "$$sum_dur"("$$user_durs1"."dur_value") AS "$$val9" FROM "user_durs" AS "$$user_durs1" WHERE ("$$user_durs1"."user_key" = "$$users0"."id")) AS "s" FROM "users" AS "$$users0""#,
    );
    assert!(params.is_empty());
}

#[test]
fn agg_avg_instant_custom() {
    let (sql, params) = sql("users { a = whens.avg() }");
    assert_sql_eq(
        &sql,
        r#"SELECT (SELECT "$$avg_instant"("$$user_whens1"."when_value") AS "$$val9" FROM "user_whens" AS "$$user_whens1" WHERE ("$$user_whens1"."user_key" = "$$users0"."id")) AS "a" FROM "users" AS "$$users0""#,
    );
    assert!(params.is_empty());
}

#[test]
fn agg_max_string_native() {
    let (sql, params) = sql("users { m = tags.max() }");
    assert_sql_eq(
        &sql,
        r#"SELECT (SELECT "MAX"("$$user_tags1"."tag_value") AS "$$val9" FROM "user_tags" AS "$$user_tags1" WHERE ("$$user_tags1"."user_key" = "$$users0"."id")) AS "m" FROM "users" AS "$$users0""#,
    );
    assert!(params.is_empty());
}

#[test]
fn agg_len_nested_counts_outer() {
    let (sql, params) = sql("users.orders.len()");
    assert_sql_eq(
        &sql,
        r#"SELECT COUNT(*) AS "$$val0" FROM "users" AS "$$users0""#,
    );
    assert!(params.is_empty());
}

#[test]
fn agg_len_merged() {
    let (sql, params) = sql("users.orders.flatten().len()");
    assert_sql_eq(
        &sql,
        r#"SELECT COUNT(*) AS "$$val0" FROM "orders" AS "$$orders1" WHERE ("$$orders1"."user_id" IN (SELECT "$$users0"."id" FROM "users" AS "$$users0"))"#,
    );
    assert!(params.is_empty());
}

#[test]
fn agg_max_by_rscalar_window() {
    let compiled = compile("users { f = orders.max_by(total) }");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$subq2".* FROM (SELECT "$$orders1".*, "ROW_NUMBER"() OVER (PARTITION BY "$$orders1"."user_id" ORDER BY "$$orders1"."total" DESC) AS "$$rn" FROM "orders" AS "$$orders1" WHERE ("$$orders1"."user_id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"()))) AS "$$subq2" WHERE ("$$subq2"."$$rn" <= ?)"#,
    );
    assert_eq!(compiled.statements[1].params, vec![int_param(1)]);
}

#[test]
fn agg_scoped_nav_element_rooted() {
    let compiled = compile("users { s = orders.sum_over(user.age) }");
    assert_eq!(compiled.statements.len(), 1);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"SELECT (SELECT "SUM"((SELECT "$$users1"."age" FROM "users" AS "$$users1" WHERE ("$$users1"."id" = "$$orders2"."user_id"))) AS "$$val3" FROM "orders" AS "$$orders2" WHERE ("$$orders2"."user_id" = "$$users0"."id")) AS "s" FROM "users" AS "$$users0""#,
    );
}

#[test]
fn sort_rejects_non_records() {
    check_rejects(&[
        ("users.orders.sort(total).flatten()", "QT0003"),
        ("users.orders.sort_desc(total)", "QT0003"),
        ("users { x = scores.sort(age) }", "QT0004"),
        (
            "users { x = [age, age + 1].take(1).sort_asc(age) }",
            "QT0004",
        ),
    ]);
}

#[test]
fn nested_contains() {
    let (sql, params) = sql("users.orders.contains(users.orders.first().unwrap())");
    assert_sql_eq(
        &sql,
        r#"SELECT EXISTS (SELECT "$$users18".* FROM "users" AS "$$users18" WHERE (((SELECT COUNT(*) AS "$$val12" FROM (SELECT "$$subq3"."total", "$$subq3"."user_id", "$$subq3"."id", "ROW_NUMBER"() OVER () AS "$$val2" FROM (SELECT "$$orders1"."total", "$$orders1"."user_id", "$$orders1"."id" FROM "orders" AS "$$orders1" WHERE ("$$orders1"."user_id" = "$$users18"."id")) AS "$$subq3") AS "$$subq13") = (SELECT COUNT(*) AS "$$val14" FROM (SELECT "$$subq11"."total", "$$subq11"."user_id", "$$subq11"."id", "ROW_NUMBER"() OVER () AS "$$val10" FROM (SELECT "$$subq9"."total", "$$subq9"."user_id", "$$subq9"."id" FROM (SELECT "$$subq7".*, ? AS "$$abort" FROM (SELECT "$$orders6"."total", "$$orders6"."user_id", "$$orders6"."id" FROM "orders" AS "$$orders6" WHERE ("$$orders6"."user_id" = (SELECT "$$users4"."id" AS "$$val5" FROM "users" AS "$$users4" LIMIT ?))) AS "$$subq7" UNION ALL SELECT ? AS "c0", ? AS "c1", ? AS "c2", ? AS "$$abort" WHERE ((CASE WHEN NOT EXISTS (SELECT ? AS "c0" FROM (SELECT "$$subq7".* FROM (SELECT "$$orders6"."total", "$$orders6"."user_id", "$$orders6"."id" FROM "orders" AS "$$orders6" WHERE ("$$orders6"."user_id" = (SELECT "$$users4"."id" AS "$$val5" FROM "users" AS "$$users4" LIMIT ?))) AS "$$subq7") AS "$$subq8") THEN "$$panic"(?) ELSE ? END) IS NOT ?)) AS "$$subq9") AS "$$subq11") AS "$$subq15")) AND NOT EXISTS (SELECT "$$subq17".* FROM (SELECT "$$subq3"."total", "$$subq3"."user_id", "$$subq3"."id", "ROW_NUMBER"() OVER () AS "$$val2" FROM (SELECT "$$orders1"."total", "$$orders1"."user_id", "$$orders1"."id" FROM "orders" AS "$$orders1" WHERE ("$$orders1"."user_id" = "$$users18"."id")) AS "$$subq3") AS "$$subq17" WHERE NOT EXISTS (SELECT "$$subq16".* FROM (SELECT "$$subq11"."total", "$$subq11"."user_id", "$$subq11"."id", "ROW_NUMBER"() OVER () AS "$$val10" FROM (SELECT "$$subq9"."total", "$$subq9"."user_id", "$$subq9"."id" FROM (SELECT "$$subq7".*, ? AS "$$abort" FROM (SELECT "$$orders6"."total", "$$orders6"."user_id", "$$orders6"."id" FROM "orders" AS "$$orders6" WHERE ("$$orders6"."user_id" = (SELECT "$$users4"."id" AS "$$val5" FROM "users" AS "$$users4" LIMIT ?))) AS "$$subq7" UNION ALL SELECT ? AS "c0", ? AS "c1", ? AS "c2", ? AS "$$abort" WHERE ((CASE WHEN NOT EXISTS (SELECT ? AS "c0" FROM (SELECT "$$subq7".* FROM (SELECT "$$orders6"."total", "$$orders6"."user_id", "$$orders6"."id" FROM "orders" AS "$$orders6" WHERE ("$$orders6"."user_id" = (SELECT "$$users4"."id" AS "$$val5" FROM "users" AS "$$users4" LIMIT ?))) AS "$$subq7") AS "$$subq8") THEN "$$panic"(?) ELSE ? END) IS NOT ?)) AS "$$subq9") AS "$$subq11") AS "$$subq16" WHERE ("$$subq16"."$$val10" = "$$subq17"."$$val2") AND ("$$subq16"."total" IS "$$subq17"."total") AND ("$$subq16"."user_id" IS "$$subq17"."user_id") AND ("$$subq16"."id" IS "$$subq17"."id"))))) AS "$$val0""#,
    );
    assert_eq!(
        params,
        vec![
            Param::Value(ParamValue::Null),
            int_param(1),
            Param::Value(ParamValue::Null),
            Param::Value(ParamValue::Null),
            Param::Value(ParamValue::Null),
            Param::Value(ParamValue::Null),
            int_param(1),
            int_param(1),
            str_param("unwrap failed: value was none"),
            Param::Value(ParamValue::Null),
            Param::Value(ParamValue::Null),
            Param::Value(ParamValue::Null),
            int_param(1),
            Param::Value(ParamValue::Null),
            Param::Value(ParamValue::Null),
            Param::Value(ParamValue::Null),
            Param::Value(ParamValue::Null),
            int_param(1),
            int_param(1),
            str_param("unwrap failed: value was none"),
            Param::Value(ParamValue::Null),
            Param::Value(ParamValue::Null),
        ]
    );
}

#[test]
fn union_middle_filter_tail() {
    use crate::codegen::model::{Cardinality, ChildRef, ParentRef, ResultShape};
    let compiled =
        compile("users { x = (if active { configs } else { configs }).filter(name == \"n1\") }");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$subq4"."name", "$$subq4"."owner", "$$subq4"."$$pkey" FROM (SELECT "$$configs2"."name", "$$configs2"."owner", "$$split1"."$$key" AS "$$pkey" FROM "$$split1" AS "$$split1", "configs" AS "$$configs2" WHERE "$$split1"."active" UNION ALL SELECT "$$configs3"."name", "$$configs3"."owner", "$$split1"."$$key" AS "$$pkey" FROM "$$split1" AS "$$split1", "configs" AS "$$configs3" WHERE NOT "$$split1"."active") AS "$$subq4" WHERE ("$$subq4"."name" = ?)"#,
    );
    assert_eq!(compiled.statements[2].params, vec![str_param("n1")]);
    let middle = ResultShape::List {
        element: Box::new(ResultShape::Record(vec![
            ("name".to_string(), ResultShape::Column("name".to_string())),
            (
                "owner".to_string(),
                ResultShape::Column("owner".to_string()),
            ),
        ])),
        child: ChildRef {
            stmt: 2,
            parent: Some(ParentRef {
                stmt: 0,
                parent_key: "$$key".to_string(),
                child_key: "$$pkey".to_string(),
                negated: false,
            }),
            cardinality: Cardinality::Many,
            tag: None,
        },
    };
    assert_eq!(
        compiled.result,
        ResultShape::List {
            element: Box::new(ResultShape::Record(vec![("x".to_string(), middle,)])),
            child: ChildRef {
                stmt: 1,
                parent: None,
                cardinality: Cardinality::Many,
                tag: None,
            },
        }
    );
}

#[test]
fn union_middle_sort_tail() {
    use crate::codegen::model::{Cardinality, ChildRef, ParentRef, ResultShape};
    let compiled = compile("users { x = (if active { configs } else { configs }).sort(name) }");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$subq4"."name", "$$subq4"."owner", "$$subq4"."$$pkey" FROM (SELECT "$$configs2"."name", "$$configs2"."owner", "$$split1"."$$key" AS "$$pkey" FROM "$$split1" AS "$$split1", "configs" AS "$$configs2" WHERE "$$split1"."active" UNION ALL SELECT "$$configs3"."name", "$$configs3"."owner", "$$split1"."$$key" AS "$$pkey" FROM "$$split1" AS "$$split1", "configs" AS "$$configs3" WHERE NOT "$$split1"."active") AS "$$subq4" ORDER BY "$$subq4"."$$pkey" ASC, "$$subq4"."name" ASC"#,
    );
    assert!(compiled.statements[2].params.is_empty());
    let middle = ResultShape::List {
        element: Box::new(ResultShape::Record(vec![
            ("name".to_string(), ResultShape::Column("name".to_string())),
            (
                "owner".to_string(),
                ResultShape::Column("owner".to_string()),
            ),
        ])),
        child: ChildRef {
            stmt: 2,
            parent: Some(ParentRef {
                stmt: 0,
                parent_key: "$$key".to_string(),
                child_key: "$$pkey".to_string(),
                negated: false,
            }),
            cardinality: Cardinality::Many,
            tag: None,
        },
    };
    assert_eq!(
        compiled.result,
        ResultShape::List {
            element: Box::new(ResultShape::Record(vec![("x".to_string(), middle,)])),
            child: ChildRef {
                stmt: 1,
                parent: None,
                cardinality: Cardinality::Many,
                tag: None,
            },
        }
    );
}

#[test]
fn union_middle_take_tail() {
    use crate::codegen::model::{Cardinality, ChildRef, ParentRef, ResultShape};
    let compiled = compile("users { x = (if active { configs } else { configs }).take(1) }");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$subq5"."name", "$$subq5"."owner", "$$subq5"."$$pkey" FROM (SELECT "$$subq4"."name", "$$subq4"."owner", "$$subq4"."$$pkey", "ROW_NUMBER"() OVER (PARTITION BY "$$subq4"."$$pkey") AS "$$rn" FROM (SELECT "$$configs2"."name", "$$configs2"."owner", "$$split1"."$$key" AS "$$pkey" FROM "$$split1" AS "$$split1", "configs" AS "$$configs2" WHERE "$$split1"."active" UNION ALL SELECT "$$configs3"."name", "$$configs3"."owner", "$$split1"."$$key" AS "$$pkey" FROM "$$split1" AS "$$split1", "configs" AS "$$configs3" WHERE NOT "$$split1"."active") AS "$$subq4") AS "$$subq5" WHERE ("$$subq5"."$$rn" <= (CASE WHEN ? >= ? THEN ? ELSE "$$panic"(?) END))"#,
    );
    assert_eq!(
        compiled.statements[2].params,
        vec![
            int_param(1),
            int_param(0),
            int_param(1),
            str_param("take requires a non-negative integer"),
        ]
    );
    let middle = ResultShape::List {
        element: Box::new(ResultShape::Record(vec![
            ("name".to_string(), ResultShape::Column("name".to_string())),
            (
                "owner".to_string(),
                ResultShape::Column("owner".to_string()),
            ),
        ])),
        child: ChildRef {
            stmt: 2,
            parent: Some(ParentRef {
                stmt: 0,
                parent_key: "$$key".to_string(),
                child_key: "$$pkey".to_string(),
                negated: false,
            }),
            cardinality: Cardinality::Many,
            tag: None,
        },
    };
    assert_eq!(
        compiled.result,
        ResultShape::List {
            element: Box::new(ResultShape::Record(vec![("x".to_string(), middle,)])),
            child: ChildRef {
                stmt: 1,
                parent: None,
                cardinality: Cardinality::Many,
                tag: None,
            },
        }
    );
}

#[test]
fn chained_over_collapsed() {
    let (sql, params) = sql("users.first()?.manager?.name");
    assert_sql_eq(
        &sql,
        r#"SELECT "$$users3"."name" AS "$$val0" FROM "users" AS "$$users3" WHERE ("$$users3"."id" IN (SELECT "$$subq2"."manager_id" FROM (SELECT "$$users1".* FROM "users" AS "$$users1" LIMIT ?) AS "$$subq2"))"#,
    );
    assert_eq!(params, vec![int_param(1)]);
}

#[test]
fn flatten_chained_through_flatten() {
    let (sql, params) = sql("users.orders.flatten().items.flatten()");
    assert_sql_eq(
        &sql,
        r#"SELECT "$$items2".* FROM "items" AS "$$items2" WHERE ("$$items2"."order_id" IN (SELECT "$$orders1"."id" FROM "orders" AS "$$orders1" WHERE ("$$orders1"."user_id" IN (SELECT "$$users0"."id" FROM "users" AS "$$users0"))))"#,
    );
    assert!(params.is_empty());
}

#[test]
fn chained_projection_tail_reuses() {
    let compiled = compile("users { x = orders } { y = x }");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"SELECT "$$subq2".* FROM (SELECT "$$users0"."id" FROM "users" AS "$$users0") AS "$$subq2""#,
    );
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$orders1".* FROM "orders" AS "$$orders1" WHERE ("$$orders1"."user_id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"()))"#,
    );
}

#[test]
fn chained_projection_tail_take_relowers() {
    let compiled = compile("users { x = orders } { y = x.take(1) }");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$subq3".* FROM (SELECT "$$orders2".*, "ROW_NUMBER"() OVER (PARTITION BY "$$orders2"."user_id") AS "$$rn" FROM "orders" AS "$$orders2" WHERE ("$$orders2"."user_id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"()))) AS "$$subq3" WHERE ("$$subq3"."$$rn" <= (CASE WHEN ? >= ? THEN ? ELSE "$$panic"(?) END))"#,
    );
    assert_eq!(
        compiled.statements[1].params,
        vec![
            int_param(1),
            int_param(0),
            int_param(1),
            Param::Value(ParamValue::String(
                "take requires a non-negative integer".to_string()
            )),
        ]
    );
}

#[test]
fn projected_list_agg_relowers() {
    let compiled = compile("users { x = scores } { y = x.sum() }");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"SELECT (SELECT "SUM"("$$user_scores2"."score_value") AS "$$val3" FROM "user_scores" AS "$$user_scores2" WHERE ("$$user_scores2"."user_key" = "$$subq4"."id")) AS "y" FROM (SELECT "$$users0"."id" FROM "users" AS "$$users0") AS "$$subq4""#,
    );
}

#[test]
fn per_row_take_computed_bound() {
    let compiled = compile("users { x = configs.take(age + 1) }");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"CREATE TEMP TABLE "$$split1" AS SELECT "$$users0".*, "$$add"("$$users0"."age", ?) AS "$$tbound", "ROW_NUMBER"() OVER () AS "$$key" FROM "users" AS "$$users0""#,
    );
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$subq4".* FROM (SELECT "$$subq3".*, "ROW_NUMBER"() OVER (PARTITION BY "$$subq3"."$$pkey") AS "$$rn" FROM (SELECT "$$configs2"."name", "$$configs2"."owner", "$$split1"."$$key" AS "$$pkey", "$$split1"."$$tbound" AS "$$tbound" FROM "$$split1" AS "$$split1", "configs" AS "$$configs2") AS "$$subq3") AS "$$subq4" WHERE ("$$subq4"."$$rn" <= (CASE WHEN "$$subq4"."$$tbound" >= ? THEN "$$subq4"."$$tbound" ELSE "$$panic"(?) END))"#,
    );
    assert_eq!(compiled.statements[0].params, vec![int_param(1)]);
}

#[test]
fn per_row_take_nav_bound() {
    let compiled = compile("users { x = configs.take(manager.age) }");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"CREATE TEMP TABLE "$$split2" AS SELECT "$$users0".*, (SELECT "$$users1"."age" FROM "users" AS "$$users1" WHERE ("$$users1"."id" = "$$users0"."manager_id")) AS "$$tbound", "ROW_NUMBER"() OVER () AS "$$key" FROM "users" AS "$$users0""#,
    );
}

#[test]
fn per_row_take_stack_folds() {
    let compiled = compile("users { x = configs.take(1).take(age) }");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"CREATE TEMP TABLE "$$split1" AS SELECT "$$users0".*, ? AS "$$tbound", "ROW_NUMBER"() OVER () AS "$$key" FROM "users" AS "$$users0""#,
    );
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$subq4".* FROM (SELECT "$$subq3".*, "ROW_NUMBER"() OVER (PARTITION BY "$$subq3"."$$pkey") AS "$$rn" FROM (SELECT "$$configs2"."name", "$$configs2"."owner", "$$split1"."$$key" AS "$$pkey", "$$split1"."$$tbound" AS "$$tbound", "$$split1"."age" AS "$$tbound1" FROM "$$split1" AS "$$split1", "configs" AS "$$configs2") AS "$$subq3") AS "$$subq4" WHERE ("$$subq4"."$$rn" <= (CASE WHEN "$$subq4"."$$tbound" >= ? THEN "$$subq4"."$$tbound" ELSE "$$panic"(?) END)) AND ("$$subq4"."$$rn" <= (CASE WHEN "$$subq4"."$$tbound1" >= ? THEN "$$subq4"."$$tbound1" ELSE "$$panic"(?) END))"#,
    );
    assert_eq!(compiled.statements[0].params, vec![int_param(1)]);
}

#[test]
fn per_row_take_stack_computed() {
    let compiled = compile("users { x = configs.take(age + 1).take(2) }");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"CREATE TEMP TABLE "$$split1" AS SELECT "$$users0".*, "$$add"("$$users0"."age", ?) AS "$$tbound", ? AS "$$tbound1", "ROW_NUMBER"() OVER () AS "$$key" FROM "users" AS "$$users0""#,
    );
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$subq4".* FROM (SELECT "$$subq3".*, "ROW_NUMBER"() OVER (PARTITION BY "$$subq3"."$$pkey") AS "$$rn" FROM (SELECT "$$configs2"."name", "$$configs2"."owner", "$$split1"."$$key" AS "$$pkey", "$$split1"."$$tbound" AS "$$tbound", "$$split1"."$$tbound1" AS "$$tbound1" FROM "$$split1" AS "$$split1", "configs" AS "$$configs2") AS "$$subq3") AS "$$subq4" WHERE ("$$subq4"."$$rn" <= (CASE WHEN "$$subq4"."$$tbound" >= ? THEN "$$subq4"."$$tbound" ELSE "$$panic"(?) END)) AND ("$$subq4"."$$rn" <= (CASE WHEN "$$subq4"."$$tbound1" >= ? THEN "$$subq4"."$$tbound1" ELSE "$$panic"(?) END))"#,
    );
}

#[test]
fn per_row_take_first_collapses() {
    use crate::codegen::model::{Cardinality, ChildRef, ParentRef, ResultShape};
    let compiled = compile("users { x = configs.take(age).first() }");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"CREATE TEMP TABLE "$$split1" AS SELECT "$$users0".*, "ROW_NUMBER"() OVER () AS "$$key" FROM "users" AS "$$users0""#,
    );
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$subq4".* FROM (SELECT "$$subq3".*, "ROW_NUMBER"() OVER (PARTITION BY "$$subq3"."$$pkey") AS "$$rn" FROM (SELECT "$$configs2"."name", "$$configs2"."owner", "$$split1"."$$key" AS "$$pkey", "$$split1"."age" AS "$$tbound" FROM "$$split1" AS "$$split1", "configs" AS "$$configs2") AS "$$subq3") AS "$$subq4" WHERE ("$$subq4"."$$rn" <= (CASE WHEN "$$subq4"."$$tbound" >= ? THEN "$$subq4"."$$tbound" ELSE "$$panic"(?) END)) AND ("$$subq4"."$$rn" <= ?)"#,
    );
    assert_eq!(
        compiled.statements[2].params,
        vec![
            int_param(0),
            Param::Value(ParamValue::String(
                "take requires a non-negative integer".to_string()
            )),
            int_param(1),
        ]
    );
    assert_eq!(
        compiled.result,
        ResultShape::List {
            element: Box::new(ResultShape::Record(vec![(
                "x".to_string(),
                ResultShape::Single {
                    element: Box::new(ResultShape::Record(vec![
                        ("name".to_string(), ResultShape::Column("name".to_string())),
                        (
                            "owner".to_string(),
                            ResultShape::Column("owner".to_string())
                        ),
                    ])),
                    child: ChildRef {
                        stmt: 2,
                        parent: Some(ParentRef {
                            stmt: 0,
                            parent_key: "$$key".to_string(),
                            child_key: "$$pkey".to_string(),
                            negated: false,
                        }),
                        cardinality: Cardinality::One,
                        tag: None,
                    },
                },
            )])),
            child: ChildRef {
                stmt: 1,
                parent: None,
                cardinality: Cardinality::Many,
                tag: None,
            },
        }
    );
}

#[test]
fn per_row_take_nth_collapses() {
    let compiled = compile("users { x = configs.take(age).nth(0) }");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"CREATE TEMP TABLE "$$split1" AS SELECT "$$users0".*, ? AS "$$tbound1", "ROW_NUMBER"() OVER () AS "$$key" FROM "users" AS "$$users0""#,
    );
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$subq4".* FROM (SELECT "$$subq3".*, "ROW_NUMBER"() OVER (PARTITION BY "$$subq3"."$$pkey") AS "$$rn" FROM (SELECT "$$configs2"."name", "$$configs2"."owner", "$$split1"."$$key" AS "$$pkey", "$$split1"."age" AS "$$tbound", "$$split1"."$$tbound1" AS "$$tbound1" FROM "$$split1" AS "$$split1", "configs" AS "$$configs2") AS "$$subq3") AS "$$subq4" WHERE ("$$subq4"."$$rn" <= (CASE WHEN "$$subq4"."$$tbound" >= ? THEN "$$subq4"."$$tbound" ELSE "$$panic"(?) END)) AND ("$$subq4"."$$rn" = "$$add"(CASE WHEN "$$subq4"."$$tbound1" >= ? THEN "$$subq4"."$$tbound1" ELSE "$$panic"(?) END, ?))"#,
    );
    assert_eq!(compiled.statements[0].params, vec![int_param(0)]);
    assert_eq!(
        compiled.statements[2].params,
        vec![
            int_param(0),
            Param::Value(ParamValue::String(
                "take requires a non-negative integer".to_string()
            )),
            int_param(0),
            Param::Value(ParamValue::String(
                "nth requires a non-negative integer".to_string()
            )),
            int_param(1),
        ]
    );
}

#[test]
fn per_row_nth_bound_collapses() {
    let compiled = compile("users { x = configs.nth(age - age) }");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"CREATE TEMP TABLE "$$split1" AS SELECT "$$users0".*, "$$sub"("$$users0"."age", "$$users0"."age") AS "$$tbound", "ROW_NUMBER"() OVER () AS "$$key" FROM "users" AS "$$users0""#,
    );
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$subq4".* FROM (SELECT "$$subq3".*, "ROW_NUMBER"() OVER (PARTITION BY "$$subq3"."$$pkey") AS "$$rn" FROM (SELECT "$$configs2"."name", "$$configs2"."owner", "$$split1"."$$key" AS "$$pkey", "$$split1"."$$tbound" AS "$$tbound" FROM "$$split1" AS "$$split1", "configs" AS "$$configs2") AS "$$subq3") AS "$$subq4" WHERE ("$$subq4"."$$rn" = "$$add"(CASE WHEN "$$subq4"."$$tbound" >= ? THEN "$$subq4"."$$tbound" ELSE "$$panic"(?) END, ?))"#,
    );
    assert_eq!(
        compiled.statements[2].params,
        vec![
            int_param(0),
            Param::Value(ParamValue::String(
                "nth requires a non-negative integer".to_string()
            )),
            int_param(1),
        ]
    );
}

#[test]
fn nth_bound_temp_index() {
    let compiled = compile("users { o = orders.nth(age - age) }");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$subq3".* FROM (SELECT "$$orders2".*, "ROW_NUMBER"() OVER (PARTITION BY "$$orders2"."user_id") AS "$$rn" FROM "orders" AS "$$orders2" WHERE ("$$orders2"."user_id" IN (SELECT "$$split1"."$$key" FROM "$$split1" AS "$$split1"))) AS "$$subq3" WHERE ("$$subq3"."$$rn" = "$$add"(CASE WHEN "$$sub"((SELECT "$$split1"."age" FROM "$$split1" AS "$$split1" WHERE ("$$split1"."$$key" = "$$subq3"."user_id")), (SELECT "$$split1"."age" FROM "$$split1" AS "$$split1" WHERE ("$$split1"."$$key" = "$$subq3"."user_id"))) >= ? THEN "$$sub"((SELECT "$$split1"."age" FROM "$$split1" AS "$$split1" WHERE ("$$split1"."$$key" = "$$subq3"."user_id")), (SELECT "$$split1"."age" FROM "$$split1" AS "$$split1" WHERE ("$$split1"."$$key" = "$$subq3"."user_id"))) ELSE "$$panic"(?) END, ?))"#,
    );
}

#[test]
fn nth_bound_nav_temp_index() {
    let compiled = compile("users { o = orders.nth(manager.age) }");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$subq4".* FROM (SELECT "$$orders2".*, "ROW_NUMBER"() OVER (PARTITION BY "$$orders2"."user_id") AS "$$rn" FROM "orders" AS "$$orders2" WHERE ("$$orders2"."user_id" IN (SELECT "$$split1"."$$key" FROM "$$split1" AS "$$split1"))) AS "$$subq4" WHERE ("$$subq4"."$$rn" = "$$add"(CASE WHEN (SELECT "$$users3"."age" FROM "users" AS "$$users3" WHERE ("$$users3"."id" = (SELECT "$$split1"."manager_id" FROM "$$split1" AS "$$split1" WHERE ("$$split1"."$$key" = "$$subq4"."user_id")))) >= ? THEN (SELECT "$$users3"."age" FROM "users" AS "$$users3" WHERE ("$$users3"."id" = (SELECT "$$split1"."manager_id" FROM "$$split1" AS "$$split1" WHERE ("$$split1"."$$key" = "$$subq4"."user_id")))) ELSE "$$panic"(?) END, ?))"#,
    );
}
#[test]
fn temp_take_first() {
    let compiled = compile("users { t = orders.take(age).first()?.total }");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"CREATE TEMP TABLE "$$split2" AS SELECT "$$users0".*, "$$users0"."id" AS "$$key" FROM "users" AS "$$users0""#,
    );
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT (SELECT "$$subq4"."total" AS "t" FROM (SELECT "$$orders3".*, "ROW_NUMBER"() OVER (PARTITION BY "$$orders3"."user_id") AS "$$rn" FROM "orders" AS "$$orders3" WHERE ("$$orders3"."user_id" IN (SELECT "$$split2"."$$key" FROM "$$split2" AS "$$split2"))) AS "$$subq4" WHERE ("$$subq4"."$$rn" <= (CASE WHEN (SELECT "$$split2"."age" FROM "$$split2" AS "$$split2" WHERE ("$$split2"."$$key" = "$$subq4"."user_id")) >= ? THEN (SELECT "$$split2"."age" FROM "$$split2" AS "$$split2" WHERE ("$$split2"."$$key" = "$$subq4"."user_id")) ELSE "$$panic"(?) END)) AND ("$$subq4"."user_id" = "$$split2"."$$key") LIMIT ?) AS "t" FROM "$$split2" AS "$$split2""#,
    );
}

#[test]
fn temp_take_predicate() {
    let compiled = compile("users.filter(orders.take(age).len() > 1) { name }");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"CREATE TEMP TABLE "$$split2" AS SELECT "$$users0".*, "$$users0"."id" AS "$$key" FROM "users" AS "$$users0""#,
    );
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$split2"."name" FROM "$$split2" AS "$$split2" WHERE ((SELECT COUNT(*) AS "$$val6" FROM (SELECT "$$subq4".* FROM (SELECT "$$orders3".*, "ROW_NUMBER"() OVER (PARTITION BY "$$orders3"."user_id") AS "$$rn" FROM "orders" AS "$$orders3" WHERE ("$$orders3"."user_id" IN (SELECT "$$split2"."$$key" FROM "$$split2" AS "$$split2"))) AS "$$subq4" WHERE ("$$subq4"."$$rn" <= (CASE WHEN (SELECT "$$split2"."age" FROM "$$split2" AS "$$split2" WHERE ("$$split2"."$$key" = "$$subq4"."user_id")) >= ? THEN (SELECT "$$split2"."age" FROM "$$split2" AS "$$split2" WHERE ("$$split2"."$$key" = "$$subq4"."user_id")) ELSE "$$panic"(?) END)) AND ("$$subq4"."user_id" = "$$split2"."$$key")) AS "$$subq5") > ?)"#,
    );
    assert_eq!(
        compiled.statements[1].params,
        vec![
            int_param(0),
            Param::Value(ParamValue::String(
                "take requires a non-negative integer".to_string()
            )),
            int_param(1),
        ]
    );
}

#[test]
fn nested_collection_temp_arm() {
    use crate::codegen::model::{Cardinality, ChildRef, ParentRef, ResultShape};
    let compiled = compile("users { o = orders { t = [items].first() } }");
    assert_eq!(compiled.statements.len(), 5);
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$orders1".* FROM "orders" AS "$$orders1" WHERE ("$$orders1"."user_id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"()))"#,
    );
    assert!(compiled.statements[2].params.is_empty());
    assert_sql_eq(
        &compiled.statements[3].sql,
        r#"SELECT "$$split4"."$$key" AS "c0", "$$split4"."id" FROM "$$split4" AS "$$split4""#,
    );
    assert!(compiled.statements[3].params.is_empty());
    assert_sql_eq(
        &compiled.statements[4].sql,
        r#"SELECT "$$split4"."$$key" AS "c0", "$$items2"."sku", "$$items2"."order_id" FROM "$$split4" AS "$$split4", "items" AS "$$items2" WHERE ("$$items2"."order_id" = "$$split4"."id")"#,
    );
    assert!(compiled.statements[4].params.is_empty());
    assert_eq!(
        compiled.result,
        ResultShape::List {
            element: Box::new(ResultShape::Record(vec![(
                "o".to_string(),
                ResultShape::List {
                    element: Box::new(ResultShape::Record(vec![(
                        "t".to_string(),
                        ResultShape::Single {
                            element: Box::new(ResultShape::List {
                                element: Box::new(ResultShape::Record(vec![
                                    ("sku".to_string(), ResultShape::Column("sku".to_string()),),
                                    (
                                        "order_id".to_string(),
                                        ResultShape::Column("order_id".to_string()),
                                    ),
                                ])),
                                child: ChildRef {
                                    stmt: 4,
                                    parent: Some(ParentRef {
                                        stmt: 3,
                                        parent_key: "c0".to_string(),
                                        child_key: "c0".to_string(),
                                        negated: false,
                                    }),
                                    cardinality: Cardinality::Many,
                                    tag: None,
                                },
                            }),
                            child: ChildRef {
                                stmt: 3,
                                parent: Some(ParentRef {
                                    stmt: 2,
                                    parent_key: "id".to_string(),
                                    child_key: "id".to_string(),
                                    negated: false,
                                }),
                                cardinality: Cardinality::One,
                                tag: None,
                            },
                        },
                    )])),
                    child: ChildRef {
                        stmt: 2,
                        parent: Some(ParentRef {
                            stmt: 1,
                            parent_key: "id".to_string(),
                            child_key: "user_id".to_string(),
                            negated: false,
                        }),
                        cardinality: Cardinality::Many,
                        tag: None,
                    },
                },
            )])),
            child: ChildRef {
                stmt: 1,
                parent: None,
                cardinality: Cardinality::Many,
                tag: None,
            },
        }
    );
}

#[test]
fn nested_collection_temp_take() {
    use crate::codegen::model::{Cardinality, ChildRef, ParentRef, ResultShape};
    let compiled = compile("users { o = orders { t = [items].take(1).first() } }");
    assert_eq!(compiled.statements.len(), 5);
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$orders1".* FROM "orders" AS "$$orders1" WHERE ("$$orders1"."user_id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"()))"#,
    );
    assert!(compiled.statements[2].params.is_empty());
    assert_sql_eq(
        &compiled.statements[3].sql,
        r#"SELECT "$$split4"."$$key" AS "c0", "$$split4"."id" FROM "$$split4" AS "$$split4""#,
    );
    assert!(compiled.statements[3].params.is_empty());
    assert_sql_eq(
        &compiled.statements[4].sql,
        r#"SELECT "$$subq6".* FROM (SELECT "$$subq5".*, "ROW_NUMBER"() OVER (PARTITION BY "$$subq5"."c0") AS "$$rn" FROM (SELECT "$$split4"."$$key" AS "c0", "$$items2"."sku", "$$items2"."order_id" FROM "$$split4" AS "$$split4", "items" AS "$$items2" WHERE ("$$items2"."order_id" = "$$split4"."id")) AS "$$subq5") AS "$$subq6" WHERE ("$$subq6"."$$rn" <= (CASE WHEN ? >= ? THEN ? ELSE "$$panic"(?) END))"#,
    );
    assert_eq!(
        compiled.statements[4].params,
        vec![
            int_param(1),
            int_param(0),
            int_param(1),
            Param::Value(ParamValue::String(
                "take requires a non-negative integer".to_string()
            )),
        ]
    );
    assert_eq!(
        compiled.result,
        ResultShape::List {
            element: Box::new(ResultShape::Record(vec![(
                "o".to_string(),
                ResultShape::List {
                    element: Box::new(ResultShape::Record(vec![(
                        "t".to_string(),
                        ResultShape::Single {
                            element: Box::new(ResultShape::List {
                                element: Box::new(ResultShape::Record(vec![
                                    ("sku".to_string(), ResultShape::Column("sku".to_string()),),
                                    (
                                        "order_id".to_string(),
                                        ResultShape::Column("order_id".to_string()),
                                    ),
                                ])),
                                child: ChildRef {
                                    stmt: 4,
                                    parent: Some(ParentRef {
                                        stmt: 3,
                                        parent_key: "c0".to_string(),
                                        child_key: "c0".to_string(),
                                        negated: false,
                                    }),
                                    cardinality: Cardinality::Many,
                                    tag: None,
                                },
                            }),
                            child: ChildRef {
                                stmt: 3,
                                parent: Some(ParentRef {
                                    stmt: 2,
                                    parent_key: "id".to_string(),
                                    child_key: "id".to_string(),
                                    negated: false,
                                }),
                                cardinality: Cardinality::One,
                                tag: None,
                            },
                        },
                    )])),
                    child: ChildRef {
                        stmt: 2,
                        parent: Some(ParentRef {
                            stmt: 1,
                            parent_key: "id".to_string(),
                            child_key: "user_id".to_string(),
                            negated: false,
                        }),
                        cardinality: Cardinality::Many,
                        tag: None,
                    },
                },
            )])),
            child: ChildRef {
                stmt: 1,
                parent: None,
                cardinality: Cardinality::Many,
                tag: None,
            },
        }
    );
}

#[test]
fn nested_cross_root_scope_inner() {
    use crate::codegen::model::{Cardinality, ChildRef, ParentRef, ResultShape};
    let compiled = compile("users { x = orders { s = subordinates.orders } }");
    assert_eq!(compiled.statements.len(), 4);
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$subq4"."user_id" FROM (SELECT "$$orders1"."user_id" FROM "orders" AS "$$orders1" WHERE ("$$orders1"."user_id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"()))) AS "$$subq4""#,
    );
    assert!(compiled.statements[1].params.is_empty());
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$users2".* FROM "users" AS "$$users2" WHERE ("$$users2"."manager_id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"()))"#,
    );
    assert!(compiled.statements[2].params.is_empty());
    assert_sql_eq(
        &compiled.statements[3].sql,
        r#"SELECT "$$orders3".* FROM "orders" AS "$$orders3" WHERE ("$$orders3"."user_id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"()))"#,
    );
    assert!(compiled.statements[3].params.is_empty());
    assert_eq!(
        compiled.result,
        ResultShape::List {
            element: Box::new(ResultShape::Record(vec![(
                "x".to_string(),
                ResultShape::List {
                    element: Box::new(ResultShape::Record(vec![(
                        "s".to_string(),
                        ResultShape::List {
                            element: Box::new(ResultShape::List {
                                element: Box::new(ResultShape::Record(vec![(
                                    "total".to_string(),
                                    ResultShape::Column("total".to_string()),
                                )])),
                                child: ChildRef {
                                    stmt: 3,
                                    parent: Some(ParentRef {
                                        stmt: 2,
                                        parent_key: "id".to_string(),
                                        child_key: "user_id".to_string(),
                                        negated: false,
                                    }),
                                    cardinality: Cardinality::Many,
                                    tag: None,
                                },
                            }),
                            child: ChildRef {
                                stmt: 2,
                                parent: Some(ParentRef {
                                    stmt: 1,
                                    parent_key: "user_id".to_string(),
                                    child_key: "manager_id".to_string(),
                                    negated: false,
                                }),
                                cardinality: Cardinality::Many,
                                tag: None,
                            },
                        },
                    )])),
                    child: ChildRef {
                        stmt: 1,
                        parent: Some(ParentRef {
                            stmt: 0,
                            parent_key: "id".to_string(),
                            child_key: "user_id".to_string(),
                            negated: false,
                        }),
                        cardinality: Cardinality::Many,
                        tag: None,
                    },
                },
            )])),
            child: ChildRef {
                stmt: 0,
                parent: None,
                cardinality: Cardinality::Many,
                tag: None,
            },
        }
    );
}

#[test]
fn nested_cross_root_inner() {
    use crate::codegen::model::{Cardinality, ChildRef, ParentRef, ResultShape};
    let compiled = compile("users { x = orders { s = orders } }");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$subq3"."user_id" FROM (SELECT "$$orders1"."user_id" FROM "orders" AS "$$orders1" WHERE ("$$orders1"."user_id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"()))) AS "$$subq3""#,
    );
    assert!(compiled.statements[1].params.is_empty());
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$orders2".* FROM "orders" AS "$$orders2" WHERE ("$$orders2"."user_id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"()))"#,
    );
    assert!(compiled.statements[2].params.is_empty());
    assert_eq!(
        compiled.result,
        ResultShape::List {
            element: Box::new(ResultShape::Record(vec![(
                "x".to_string(),
                ResultShape::List {
                    element: Box::new(ResultShape::Record(vec![(
                        "s".to_string(),
                        ResultShape::List {
                            element: Box::new(ResultShape::Record(vec![(
                                "total".to_string(),
                                ResultShape::Column("total".to_string()),
                            )])),
                            child: ChildRef {
                                stmt: 2,
                                parent: Some(ParentRef {
                                    stmt: 1,
                                    parent_key: "user_id".to_string(),
                                    child_key: "user_id".to_string(),
                                    negated: false,
                                }),
                                cardinality: Cardinality::Many,
                                tag: None,
                            },
                        },
                    )])),
                    child: ChildRef {
                        stmt: 1,
                        parent: Some(ParentRef {
                            stmt: 0,
                            parent_key: "id".to_string(),
                            child_key: "user_id".to_string(),
                            negated: false,
                        }),
                        cardinality: Cardinality::Many,
                        tag: None,
                    },
                },
            )])),
            child: ChildRef {
                stmt: 0,
                parent: None,
                cardinality: Cardinality::Many,
                tag: None,
            },
        }
    );
}

#[test]
fn nested_take_bound_middle() {
    use crate::codegen::model::{Cardinality, ChildRef, ParentRef, ResultShape};
    let compiled = compile("users { o = orders.take(age) { s = items } }");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$subq3"."id", "$$subq3"."user_id" FROM (SELECT "$$orders1".*, "ROW_NUMBER"() OVER (PARTITION BY "$$orders1"."user_id") AS "$$rn" FROM "orders" AS "$$orders1" WHERE ("$$orders1"."user_id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"()))) AS "$$subq3" WHERE ("$$subq3"."$$rn" <= (CASE WHEN (SELECT "$$users2"."age" FROM "users" AS "$$users2" WHERE ("$$users2"."id" = "$$subq3"."user_id")) >= ? THEN (SELECT "$$users2"."age" FROM "users" AS "$$users2" WHERE ("$$users2"."id" = "$$subq3"."user_id")) ELSE "$$panic"(?) END))"#,
    );
    assert_eq!(
        compiled.statements[1].params,
        vec![
            int_param(0),
            Param::Value(ParamValue::String(
                "take requires a non-negative integer".to_string()
            )),
        ]
    );
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$items4".* FROM "items" AS "$$items4" WHERE ("$$items4"."order_id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"()))"#,
    );
    assert!(compiled.statements[2].params.is_empty());
    assert_eq!(
        compiled.result,
        ResultShape::List {
            element: Box::new(ResultShape::Record(vec![(
                "o".to_string(),
                ResultShape::List {
                    element: Box::new(ResultShape::Record(vec![(
                        "s".to_string(),
                        ResultShape::List {
                            element: Box::new(ResultShape::Record(vec![(
                                "sku".to_string(),
                                ResultShape::Column("sku".to_string()),
                            )])),
                            child: ChildRef {
                                stmt: 2,
                                parent: Some(ParentRef {
                                    stmt: 1,
                                    parent_key: "id".to_string(),
                                    child_key: "order_id".to_string(),
                                    negated: false,
                                }),
                                cardinality: Cardinality::Many,
                                tag: None,
                            },
                        },
                    )])),
                    child: ChildRef {
                        stmt: 1,
                        parent: Some(ParentRef {
                            stmt: 0,
                            parent_key: "id".to_string(),
                            child_key: "user_id".to_string(),
                            negated: false,
                        }),
                        cardinality: Cardinality::Many,
                        tag: None,
                    },
                },
            )])),
            child: ChildRef {
                stmt: 0,
                parent: None,
                cardinality: Cardinality::Many,
                tag: None,
            },
        }
    );
}

#[test]
fn nested_take_bound_inner() {
    use crate::codegen::model::{Cardinality, ChildRef, ParentRef, ResultShape};
    let compiled = compile("users { o = orders { s = items.take(age) } }");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$orders1"."id", "$$orders1"."user_id" FROM "orders" AS "$$orders1" WHERE ("$$orders1"."user_id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"()))"#,
    );
    assert!(compiled.statements[1].params.is_empty());
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$subq5".* FROM (SELECT "$$items2".*, "ROW_NUMBER"() OVER (PARTITION BY "$$items2"."order_id") AS "$$rn" FROM "items" AS "$$items2" WHERE ("$$items2"."order_id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"()))) AS "$$subq5" WHERE ("$$subq5"."$$rn" <= (CASE WHEN (SELECT "$$users3"."age" FROM "users" AS "$$users3" WHERE ("$$users3"."id" = (SELECT "$$orders4"."user_id" FROM "orders" AS "$$orders4" WHERE ("$$orders4"."id" = "$$subq5"."order_id")))) >= ? THEN (SELECT "$$users3"."age" FROM "users" AS "$$users3" WHERE ("$$users3"."id" = (SELECT "$$orders4"."user_id" FROM "orders" AS "$$orders4" WHERE ("$$orders4"."id" = "$$subq5"."order_id")))) ELSE "$$panic"(?) END))"#,
    );
    assert_eq!(
        compiled.statements[2].params,
        vec![
            int_param(0),
            Param::Value(ParamValue::String(
                "take requires a non-negative integer".to_string()
            )),
        ]
    );
    assert_eq!(
        compiled.result,
        ResultShape::List {
            element: Box::new(ResultShape::Record(vec![(
                "o".to_string(),
                ResultShape::List {
                    element: Box::new(ResultShape::Record(vec![(
                        "s".to_string(),
                        ResultShape::List {
                            element: Box::new(ResultShape::Record(vec![(
                                "sku".to_string(),
                                ResultShape::Column("sku".to_string()),
                            )])),
                            child: ChildRef {
                                stmt: 2,
                                parent: Some(ParentRef {
                                    stmt: 1,
                                    parent_key: "id".to_string(),
                                    child_key: "order_id".to_string(),
                                    negated: false,
                                }),
                                cardinality: Cardinality::Many,
                                tag: None,
                            },
                        },
                    )])),
                    child: ChildRef {
                        stmt: 1,
                        parent: Some(ParentRef {
                            stmt: 0,
                            parent_key: "id".to_string(),
                            child_key: "user_id".to_string(),
                            negated: false,
                        }),
                        cardinality: Cardinality::Many,
                        tag: None,
                    },
                },
            )])),
            child: ChildRef {
                stmt: 0,
                parent: None,
                cardinality: Cardinality::Many,
                tag: None,
            },
        }
    );
}

#[test]
fn temp_array_predicate_sort() {
    let compiled = compile("users.filter([age].contains(30)) { name }");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$split1"."name" FROM "$$split1" AS "$$split1" WHERE EXISTS (SELECT "$$subq2".* FROM (SELECT "$$split1"."$$key" AS "c0", "$$split1"."age" AS "c1" FROM "$$split1" AS "$$split1") AS "$$subq2" WHERE ("$$subq2"."c0" = "$$split1"."$$key") AND ("$$subq2"."c1" = ?))"#,
    );
    let compiled = compile("users.sort([age, age + 1].take(age - 30).len()) { name }");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$split1"."name" FROM "$$split1" AS "$$split1" ORDER BY (SELECT COUNT(*) AS "$$val5" FROM (SELECT "$$subq3".* FROM (SELECT "$$subq2".*, "ROW_NUMBER"() OVER (PARTITION BY "$$subq2"."c0") AS "$$rn" FROM (SELECT "$$split1"."$$key" AS "c0", "$$split1"."age" AS "c1" FROM "$$split1" AS "$$split1" UNION ALL SELECT "$$split1"."$$key" AS "c0", "$$add"("$$split1"."age", ?) AS "c1" FROM "$$split1" AS "$$split1") AS "$$subq2") AS "$$subq3" WHERE ("$$subq3"."$$rn" <= (CASE WHEN "$$sub"((SELECT "$$split1"."age" FROM "$$split1" AS "$$split1" WHERE ("$$split1"."$$key" = "$$subq3"."c0")), ?) >= ? THEN "$$sub"((SELECT "$$split1"."age" FROM "$$split1" AS "$$split1" WHERE ("$$split1"."$$key" = "$$subq3"."c0")), ?) ELSE "$$panic"(?) END))) AS "$$subq4" WHERE ("$$subq4"."c0" = "$$split1"."$$key")) ASC"#,
    );
}

#[test]
fn contains_scope_needle() {
    let (sql_storage, _) = sql(r#"users[tags.contains(name)] { name }"#);
    assert_sql_eq(
        &sql_storage,
        r#"SELECT "$$users0"."name" FROM "users" AS "$$users0" WHERE EXISTS (SELECT "$$user_tags1"."user_key" FROM "user_tags" AS "$$user_tags1" WHERE ("$$user_tags1"."user_key" = "$$users0"."id") AND ("$$user_tags1"."tag_value" = "$$users0"."name"))"#,
    );
    let (sql_narrow, _) = sql(r#"users[tags.skip(1).contains(name)] { name }"#);
    assert_sql_eq(
        &sql_narrow,
        r#"SELECT "$$users0"."name" FROM "users" AS "$$users0" WHERE EXISTS (SELECT "$$subq2".* FROM (SELECT "$$user_tags1"."tag_value" FROM "user_tags" AS "$$user_tags1" WHERE ("$$user_tags1"."user_key" = "$$users0"."id") LIMIT -1 OFFSET (CASE WHEN ? >= ? THEN ? ELSE "$$panic"(?) END)) AS "$$subq2" WHERE ("$$subq2"."tag_value" = "$$users0"."name"))"#,
    );
}

#[test]
fn temp_take_sort_key() {
    let compiled = compile("users.sort(orders.take(age).len()) { name }");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"CREATE TEMP TABLE "$$split2" AS SELECT "$$users0".*, "$$users0"."id" AS "$$key" FROM "users" AS "$$users0""#,
    );
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$split2"."name" FROM "$$split2" AS "$$split2" ORDER BY (SELECT COUNT(*) AS "$$val6" FROM (SELECT "$$subq4".* FROM (SELECT "$$orders3".*, "ROW_NUMBER"() OVER (PARTITION BY "$$orders3"."user_id") AS "$$rn" FROM "orders" AS "$$orders3" WHERE ("$$orders3"."user_id" IN (SELECT "$$split2"."$$key" FROM "$$split2" AS "$$split2"))) AS "$$subq4" WHERE ("$$subq4"."$$rn" <= (CASE WHEN (SELECT "$$split2"."age" FROM "$$split2" AS "$$split2" WHERE ("$$split2"."$$key" = "$$subq4"."user_id")) >= ? THEN (SELECT "$$split2"."age" FROM "$$split2" AS "$$split2" WHERE ("$$split2"."$$key" = "$$subq4"."user_id")) ELSE "$$panic"(?) END)) AND ("$$subq4"."user_id" = "$$split2"."$$key")) AS "$$subq5") ASC"#,
    );
}

#[test]
fn temp_take_sum_over() {
    let compiled = compile("users { s = orders.take(age).sum_over(total) }");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"CREATE TEMP TABLE "$$split2" AS SELECT "$$users0".*, "$$users0"."id" AS "$$key" FROM "users" AS "$$users0""#,
    );
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT (SELECT "$$sum_dec"("$$subq4"."total") AS "$$val5" FROM (SELECT "$$subq3".* FROM (SELECT "$$orders1".*, "ROW_NUMBER"() OVER (PARTITION BY "$$orders1"."user_id") AS "$$rn" FROM "orders" AS "$$orders1" WHERE ("$$orders1"."user_id" IN (SELECT "$$split2"."$$key" FROM "$$split2" AS "$$split2"))) AS "$$subq3" WHERE ("$$subq3"."$$rn" <= (CASE WHEN (SELECT "$$split2"."age" FROM "$$split2" AS "$$split2" WHERE ("$$split2"."$$key" = "$$subq3"."user_id")) >= ? THEN (SELECT "$$split2"."age" FROM "$$split2" AS "$$split2" WHERE ("$$split2"."$$key" = "$$subq3"."user_id")) ELSE "$$panic"(?) END)) AND ("$$subq3"."user_id" = "$$split2"."$$key")) AS "$$subq4") AS "s" FROM "$$split2" AS "$$split2""#,
    );
    assert_eq!(
        compiled.statements[1].params,
        vec![
            int_param(0),
            Param::Value(ParamValue::String(
                "take requires a non-negative integer".to_string()
            )),
        ]
    );
}

#[test]
fn temp_take_sum_over_outer() {
    let compiled = compile("users { s = orders.take(age).sum_over(manager.age) }");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT (SELECT "SUM"((SELECT "$$users4"."age" FROM "users" AS "$$users4" WHERE ("$$users4"."id" = (SELECT "$$split2"."manager_id" FROM "$$split2" AS "$$split2" WHERE ("$$split2"."$$key" = "$$subq6"."user_id"))))) AS "$$val7" FROM (SELECT "$$subq5".* FROM (SELECT "$$orders1".*, "ROW_NUMBER"() OVER (PARTITION BY "$$orders1"."user_id") AS "$$rn" FROM "orders" AS "$$orders1" WHERE ("$$orders1"."user_id" IN (SELECT "$$split2"."$$key" FROM "$$split2" AS "$$split2"))) AS "$$subq5" WHERE ("$$subq5"."$$rn" <= (CASE WHEN (SELECT "$$split2"."age" FROM "$$split2" AS "$$split2" WHERE ("$$split2"."$$key" = "$$subq5"."user_id")) >= ? THEN (SELECT "$$split2"."age" FROM "$$split2" AS "$$split2" WHERE ("$$split2"."$$key" = "$$subq5"."user_id")) ELSE "$$panic"(?) END)) AND ("$$subq5"."user_id" = "$$split2"."$$key")) AS "$$subq6") AS "s" FROM "$$split2" AS "$$split2""#,
    );
}

#[test]
fn temp_take_len() {
    let compiled = compile("users { t = orders.take(age).len() }");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT (SELECT COUNT(*) AS "$$val5" FROM (SELECT "$$subq3".* FROM (SELECT "$$orders1".*, "ROW_NUMBER"() OVER (PARTITION BY "$$orders1"."user_id") AS "$$rn" FROM "orders" AS "$$orders1" WHERE ("$$orders1"."user_id" IN (SELECT "$$split2"."$$key" FROM "$$split2" AS "$$split2"))) AS "$$subq3" WHERE ("$$subq3"."$$rn" <= (CASE WHEN (SELECT "$$split2"."age" FROM "$$split2" AS "$$split2" WHERE ("$$split2"."$$key" = "$$subq3"."user_id")) >= ? THEN (SELECT "$$split2"."age" FROM "$$split2" AS "$$split2" WHERE ("$$split2"."$$key" = "$$subq3"."user_id")) ELSE "$$panic"(?) END)) AND ("$$subq3"."user_id" = "$$split2"."$$key")) AS "$$subq4") AS "t" FROM "$$split2" AS "$$split2""#,
    );
}

#[test]
fn sum_over_outer_nav() {
    let compiled = compile("users { s = orders.sum_over(manager.age) }");
    assert_eq!(compiled.statements.len(), 1);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"SELECT (SELECT "SUM"((SELECT "$$users3"."age" AS "$$val4" FROM "users" AS "$$users2", "users" AS "$$users3" WHERE ("$$users3"."id" = "$$users2"."manager_id") AND ("$$users2"."id" = "$$orders5"."user_id"))) AS "$$val6" FROM "orders" AS "$$orders5" WHERE ("$$orders5"."user_id" = "$$users0"."id")) AS "s" FROM "users" AS "$$users0""#,
    );
}

#[test]
fn sum_over_outer_bare() {
    let compiled = compile("users { s = orders.sum_over(age) }");
    assert_eq!(compiled.statements.len(), 1);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"SELECT (SELECT "SUM"((SELECT "$$users3"."age" AS "$$val4" FROM "users" AS "$$users3" WHERE ("$$users3"."id" = "$$orders5"."user_id"))) AS "$$val6" FROM "orders" AS "$$orders5" WHERE ("$$orders5"."user_id" = "$$users0"."id")) AS "s" FROM "users" AS "$$users0""#,
    );
}

#[test]
fn sum_over_collection_temp_nested() {
    let compiled = compile("users { s = orders.take(age).sum_over(manager.orders.len()) }");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"CREATE TEMP TABLE "$$split2" AS SELECT "$$users0".*, "$$users0"."id" AS "$$key" FROM "users" AS "$$users0""#,
    );
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT (SELECT "SUM"((SELECT COUNT(*) AS "$$val7" FROM "orders" AS "$$orders6" WHERE ("$$orders6"."user_id" = (SELECT "$$split2"."manager_id" FROM "$$split2" AS "$$split2" WHERE ("$$split2"."$$key" = "$$subq9"."user_id"))))) AS "$$val10" FROM (SELECT "$$subq8".* FROM (SELECT "$$orders3".*, "ROW_NUMBER"() OVER (PARTITION BY "$$orders3"."user_id") AS "$$rn" FROM "orders" AS "$$orders3" WHERE ("$$orders3"."user_id" IN (SELECT "$$split2"."$$key" FROM "$$split2" AS "$$split2"))) AS "$$subq8" WHERE ("$$subq8"."$$rn" <= (CASE WHEN (SELECT "$$split2"."age" FROM "$$split2" AS "$$split2" WHERE ("$$split2"."$$key" = "$$subq8"."user_id")) >= ? THEN (SELECT "$$split2"."age" FROM "$$split2" AS "$$split2" WHERE ("$$split2"."$$key" = "$$subq8"."user_id")) ELSE "$$panic"(?) END)) AND ("$$subq8"."user_id" = "$$split2"."$$key")) AS "$$subq9") AS "s" FROM "$$split2" AS "$$split2""#,
    );
}

#[test]
fn sum_over_collection_tailed() {
    let compiled = compile("users { s = orders.sum_over(manager.orders.take(1).len()) }");
    assert_eq!(compiled.statements.len(), 1);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"SELECT (SELECT "SUM"((SELECT COUNT(*) AS "$$val6" FROM (SELECT "$$orders4".* FROM "orders" AS "$$orders4" WHERE ("$$orders4"."user_id" = (SELECT "$$users3"."manager_id" FROM "users" AS "$$users3" WHERE ("$$users3"."id" = "$$orders7"."user_id"))) LIMIT (CASE WHEN ? >= ? THEN ? ELSE "$$panic"(?) END)) AS "$$subq5")) AS "$$val8" FROM "orders" AS "$$orders7" WHERE ("$$orders7"."user_id" = "$$users0"."id")) AS "s" FROM "users" AS "$$users0""#,
    );
    let compiled = compile("users { s = orders.sum_over(manager.orders.skip(1).len()) }");
    assert_eq!(compiled.statements.len(), 1);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"SELECT (SELECT "SUM"((SELECT COUNT(*) AS "$$val6" FROM (SELECT "$$orders4".* FROM "orders" AS "$$orders4" WHERE ("$$orders4"."user_id" = (SELECT "$$users3"."manager_id" FROM "users" AS "$$users3" WHERE ("$$users3"."id" = "$$orders7"."user_id"))) LIMIT -1 OFFSET (CASE WHEN ? >= ? THEN ? ELSE "$$panic"(?) END)) AS "$$subq5")) AS "$$val8" FROM "orders" AS "$$orders7" WHERE ("$$orders7"."user_id" = "$$users0"."id")) AS "s" FROM "users" AS "$$users0""#,
    );
}

#[test]
fn sum_over_collection_deeper() {
    let compiled = compile("users { s = orders.sum_over(manager.manager.orders.len()) }");
    assert_eq!(compiled.statements.len(), 1);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"SELECT (SELECT "SUM"((SELECT COUNT(*) AS "$$val6" FROM "orders" AS "$$orders5" WHERE ("$$orders5"."user_id" = (SELECT "$$users4"."manager_id" FROM "users" AS "$$users4" WHERE ("$$users4"."id" = (SELECT "$$users3"."manager_id" FROM "users" AS "$$users3" WHERE ("$$users3"."id" = "$$orders7"."user_id"))))))) AS "$$val8" FROM "orders" AS "$$orders7" WHERE ("$$orders7"."user_id" = "$$users0"."id")) AS "s" FROM "users" AS "$$users0""#,
    );
}

#[test]
fn sum_over_collection_nested() {
    let compiled = compile("users { s = orders.sum_over(manager.orders.len()) }");
    assert_eq!(compiled.statements.len(), 1);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"SELECT (SELECT "SUM"((SELECT COUNT(*) AS "$$val7" FROM "orders" AS "$$orders6" WHERE ("$$orders6"."user_id" = (SELECT "$$users5"."manager_id" FROM "users" AS "$$users5" WHERE ("$$users5"."id" = "$$orders8"."user_id"))))) AS "$$val9" FROM "orders" AS "$$orders8" WHERE ("$$orders8"."user_id" = "$$users0"."id")) AS "s" FROM "users" AS "$$users0""#,
    );
    let compiled = compile("users { s = orders.sum_over(manager.tags.len()) }");
    assert_eq!(compiled.statements.len(), 1);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"SELECT (SELECT "SUM"((SELECT COUNT(*) AS "$$val7" FROM "user_tags" AS "$$user_tags6" WHERE ("$$user_tags6"."user_key" = (SELECT "$$users5"."manager_id" FROM "users" AS "$$users5" WHERE ("$$users5"."id" = "$$orders8"."user_id"))))) AS "$$val9" FROM "orders" AS "$$orders8" WHERE ("$$orders8"."user_id" = "$$users0"."id")) AS "s" FROM "users" AS "$$users0""#,
    );
    let compiled = compile("users { s = orders.sum_over(tags.len()) }");
    assert_eq!(compiled.statements.len(), 1);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"SELECT (SELECT "SUM"((SELECT COUNT(*) AS "$$val6" FROM "user_tags" AS "$$user_tags5" WHERE ("$$user_tags5"."user_key" = "$$orders7"."user_id"))) AS "$$val8" FROM "orders" AS "$$orders7" WHERE ("$$orders7"."user_id" = "$$users0"."id")) AS "s" FROM "users" AS "$$users0""#,
    );
}

#[test]
fn cross_take_outer_bound() {
    let compiled = compile("users { x = configs.filter(owner == email).take(age) }");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"CREATE TEMP TABLE "$$split1" AS SELECT "$$users0".* FROM "users" AS "$$users0""#,
    );
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$subq4".* FROM (SELECT "$$subq3".*, "ROW_NUMBER"() OVER (PARTITION BY "$$subq3"."$$pkey") AS "$$rn" FROM (SELECT "$$configs2".*, (SELECT "$$split1"."address" FROM "$$split1" AS "$$split1" WHERE ("$$split1"."address" = "$$configs2"."owner")) AS "$$pkey" FROM "configs" AS "$$configs2") AS "$$subq3" WHERE ("$$subq3"."$$pkey" IS NOT ?)) AS "$$subq4" WHERE ("$$subq4"."$$rn" <= (CASE WHEN (SELECT "$$split1"."age" FROM "$$split1" AS "$$split1" WHERE ("$$split1"."address" = "$$subq4"."$$pkey")) >= ? THEN (SELECT "$$split1"."age" FROM "$$split1" AS "$$split1" WHERE ("$$split1"."address" = "$$subq4"."$$pkey")) ELSE "$$panic"(?) END))"#,
    );
}

#[test]
fn cross_range_take_outer_bound() {
    let compiled = compile("users { x = configs.filter(owner > email).take(age) }");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"CREATE TEMP TABLE "$$split1" AS SELECT "$$users0".*, "ROW_NUMBER"() OVER () AS "$$key" FROM "users" AS "$$users0""#,
    );
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$subq4".* FROM (SELECT "$$subq3".*, "ROW_NUMBER"() OVER (PARTITION BY "$$subq3"."$$pkey") AS "$$rn" FROM (SELECT "$$configs2"."name", "$$configs2"."owner", "$$split1"."$$key" AS "$$pkey" FROM "$$split1" AS "$$split1", "configs" AS "$$configs2" WHERE ("$$split1"."address" < "$$configs2"."owner")) AS "$$subq3") AS "$$subq4" WHERE ("$$subq4"."$$rn" <= (CASE WHEN (SELECT "$$split1"."age" FROM "$$split1" AS "$$split1" WHERE ("$$split1"."$$key" = "$$subq4"."$$pkey")) >= ? THEN (SELECT "$$split1"."age" FROM "$$split1" AS "$$split1" WHERE ("$$split1"."$$key" = "$$subq4"."$$pkey")) ELSE "$$panic"(?) END))"#,
    );
}

#[test]
fn max_by_outer_nav_key() {
    let compiled = compile("users { m = orders.max_by(manager.age) }");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$subq4".* FROM (SELECT "$$orders2".*, "ROW_NUMBER"() OVER (PARTITION BY "$$orders2"."user_id" ORDER BY (SELECT "$$users3"."age" FROM "users" AS "$$users3" WHERE ("$$users3"."id" = (SELECT "$$split1"."manager_id" FROM "$$split1" AS "$$split1" WHERE ("$$split1"."$$key" = "$$orders2"."user_id")))) DESC) AS "$$rn" FROM "orders" AS "$$orders2" WHERE ("$$orders2"."user_id" IN (SELECT "$$split1"."$$key" FROM "$$split1" AS "$$split1"))) AS "$$subq4" WHERE ("$$subq4"."$$rn" <= ?)"#,
    );
}

#[test]
fn min_by_outer_nav_key() {
    let compiled = compile("users { m = orders.min_by(manager.age) }");
    assert_eq!(compiled.statements.len(), 3);
    assert_sql_eq(
        &compiled.statements[2].sql,
        r#"SELECT "$$subq4".* FROM (SELECT "$$orders2".*, "ROW_NUMBER"() OVER (PARTITION BY "$$orders2"."user_id" ORDER BY (SELECT "$$users3"."age" FROM "users" AS "$$users3" WHERE ("$$users3"."id" = (SELECT "$$split1"."manager_id" FROM "$$split1" AS "$$split1" WHERE ("$$split1"."$$key" = "$$orders2"."user_id")))) ASC) AS "$$rn" FROM "orders" AS "$$orders2" WHERE ("$$orders2"."user_id" IN (SELECT "$$split1"."$$key" FROM "$$split1" AS "$$split1"))) AS "$$subq4" WHERE ("$$subq4"."$$rn" <= ?)"#,
    );
}

#[test]
fn sum_over_mixed_element_parent() {
    let compiled = compile("users { s = orders.sum_over(user.age + manager.age) }");
    assert_eq!(compiled.statements.len(), 1);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"SELECT (SELECT "SUM"("$$add"((SELECT "$$users6"."age" FROM "users" AS "$$users6" WHERE ("$$users6"."id" = "$$orders7"."user_id")), (SELECT "$$users4"."age" AS "$$val5" FROM "users" AS "$$users3", "users" AS "$$users4" WHERE ("$$users4"."id" = "$$users3"."manager_id") AND ("$$users3"."id" = "$$orders7"."user_id")))) AS "$$val8" FROM "orders" AS "$$orders7" WHERE ("$$orders7"."user_id" = "$$users0"."id")) AS "s" FROM "users" AS "$$users0""#,
    );
}

#[test]
fn sum_over_method_outer() {
    let compiled = compile("users { s = orders.sum_over(manager.name.len()) }");
    assert_eq!(compiled.statements.len(), 1);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"SELECT (SELECT "SUM"("length"((SELECT "$$users3"."name" AS "$$val4" FROM "users" AS "$$users2", "users" AS "$$users3" WHERE ("$$users3"."id" = "$$users2"."manager_id") AND ("$$users2"."id" = "$$orders5"."user_id")))) AS "$$val6" FROM "orders" AS "$$orders5" WHERE ("$$orders5"."user_id" = "$$users0"."id")) AS "s" FROM "users" AS "$$users0""#,
    );
}

#[test]
fn sum_over_method_mixed() {
    let compiled = compile("users { s = orders.sum_over(manager.name.len() + age) }");
    assert_eq!(compiled.statements.len(), 1);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"SELECT (SELECT "SUM"("$$add"("length"((SELECT "$$users4"."name" AS "$$val5" FROM "users" AS "$$users3", "users" AS "$$users4" WHERE ("$$users4"."id" = "$$users3"."manager_id") AND ("$$users3"."id" = "$$orders8"."user_id"))), (SELECT "$$users6"."age" AS "$$val7" FROM "users" AS "$$users6" WHERE ("$$users6"."id" = "$$orders8"."user_id")))) AS "$$val9" FROM "orders" AS "$$orders8" WHERE ("$$orders8"."user_id" = "$$users0"."id")) AS "s" FROM "users" AS "$$users0""#,
    );
}

#[test]
fn sum_over_same_chain_twice() {
    let compiled = compile("users { s = orders.sum_over(manager.age + manager.age) }");
    assert_eq!(compiled.statements.len(), 1);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"SELECT (SELECT "SUM"("$$add"((SELECT "$$users4"."age" AS "$$val5" FROM "users" AS "$$users3", "users" AS "$$users4" WHERE ("$$users4"."id" = "$$users3"."manager_id") AND ("$$users3"."id" = "$$orders9"."user_id")), (SELECT "$$users7"."age" AS "$$val8" FROM "users" AS "$$users6", "users" AS "$$users7" WHERE ("$$users7"."id" = "$$users6"."manager_id") AND ("$$users6"."id" = "$$orders9"."user_id")))) AS "$$val10" FROM "orders" AS "$$orders9" WHERE ("$$orders9"."user_id" = "$$users0"."id")) AS "s" FROM "users" AS "$$users0""#,
    );
}

#[test]
fn take_sort_global_boundary() {
    let compiled = compile("configs.sort(name).take(1).sort_desc(name)");
    assert_eq!(compiled.statements.len(), 1);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"SELECT "$$subq1".* FROM (SELECT "$$configs0".* FROM "configs" AS "$$configs0" ORDER BY "$$configs0"."name" ASC LIMIT (CASE WHEN ? >= ? THEN ? ELSE "$$panic"(?) END)) AS "$$subq1" ORDER BY "$$subq1"."name" DESC"#,
    );
}

#[test]
fn take_sort_windowed_boundary() {
    let compiled = compile("users { o = orders.take(1).sort_desc(total) { total } }");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$subq2"."total", "$$subq2"."user_id" FROM (SELECT "$$orders1".*, "ROW_NUMBER"() OVER (PARTITION BY "$$orders1"."user_id") AS "$$rn" FROM "orders" AS "$$orders1" WHERE ("$$orders1"."user_id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"()))) AS "$$subq2" WHERE ("$$subq2"."$$rn" <= (CASE WHEN ? >= ? THEN ? ELSE "$$panic"(?) END)) ORDER BY "$$subq2"."total" DESC"#,
    );
}

#[test]
fn single_hop_pairs_by_middle_fk() {
    use crate::codegen::model::{Cardinality, ChildRef, ParentRef, ResultShape};
    let compiled = compile("users { o = manager.orders }");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[0].sql,
        r#"SELECT "$$users0"."manager_id" FROM "users" AS "$$users0""#,
    );
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$orders1".* FROM "orders" AS "$$orders1" WHERE ("$$orders1"."user_id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"()))"#,
    );
    assert_eq!(
        compiled.result,
        ResultShape::List {
            element: Box::new(ResultShape::Record(vec![(
                "o".to_string(),
                ResultShape::List {
                    element: Box::new(ResultShape::Record(vec![(
                        "total".to_string(),
                        ResultShape::Column("total".to_string())
                    ),])),
                    child: ChildRef {
                        stmt: 1,
                        parent: Some(ParentRef {
                            stmt: 0,
                            parent_key: "manager_id".to_string(),
                            child_key: "user_id".to_string(),
                            negated: false,
                        }),
                        cardinality: Cardinality::Many,
                        tag: None,
                    },
                }
            ),])),
            child: ChildRef {
                stmt: 0,
                parent: None,
                cardinality: Cardinality::Many,
                tag: None,
            },
        }
    );
}

#[test]
fn single_hop_take_windows_by_leaf_fk() {
    let compiled = compile("users { o = manager.orders.take(1) }");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$subq2".* FROM (SELECT "$$orders1".*, "ROW_NUMBER"() OVER (PARTITION BY "$$orders1"."user_id") AS "$$rn" FROM "orders" AS "$$orders1" WHERE ("$$orders1"."user_id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"()))) AS "$$subq2" WHERE ("$$subq2"."$$rn" <= (CASE WHEN ? >= ? THEN ? ELSE "$$panic"(?) END))"#,
    );
}

#[test]
fn single_hop_first_collapses() {
    use crate::codegen::model::{Cardinality, ChildRef, ParentRef, ResultShape};
    let compiled = compile("users { o = manager.orders.first() }");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$subq2".* FROM (SELECT "$$orders1".*, "ROW_NUMBER"() OVER (PARTITION BY "$$orders1"."user_id") AS "$$rn" FROM "orders" AS "$$orders1" WHERE ("$$orders1"."user_id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"()))) AS "$$subq2" WHERE ("$$subq2"."$$rn" <= ?)"#,
    );
    assert_eq!(compiled.statements[1].params, vec![int_param(1)]);
    assert_eq!(
        compiled.result,
        ResultShape::List {
            element: Box::new(ResultShape::Record(vec![(
                "o".to_string(),
                ResultShape::Single {
                    element: Box::new(ResultShape::Record(vec![(
                        "total".to_string(),
                        ResultShape::Column("total".to_string())
                    ),])),
                    child: ChildRef {
                        stmt: 1,
                        parent: Some(ParentRef {
                            stmt: 0,
                            parent_key: "manager_id".to_string(),
                            child_key: "user_id".to_string(),
                            negated: false,
                        }),
                        cardinality: Cardinality::One,
                        tag: None,
                    },
                }
            ),])),
            child: ChildRef {
                stmt: 0,
                parent: None,
                cardinality: Cardinality::Many,
                tag: None,
            },
        }
    );
}

#[test]
fn single_hop_nth_collapses() {
    let compiled = compile("users { o = manager.orders.nth(1) }");
    assert_eq!(compiled.statements.len(), 2);
    assert_sql_eq(
        &compiled.statements[1].sql,
        r#"SELECT "$$subq2".* FROM (SELECT "$$orders1".*, "ROW_NUMBER"() OVER (PARTITION BY "$$orders1"."user_id") AS "$$rn" FROM "orders" AS "$$orders1" WHERE ("$$orders1"."user_id" IN (SELECT "$$bulk_params"."value" FROM "$$bulk_params"()))) AS "$$subq2" WHERE ("$$subq2"."$$rn" = ?)"#,
    );
    assert_eq!(compiled.statements[1].params, vec![int_param(2)]);
}
