
use grove_runtime::{Runtime, Value};

const SCHEMA_SRC: &str = r#"
    root users: User;
    root roles: Role;
    struct User {
        name: String,
        age: Int,
        roles: &List<Role>,
    }
    struct Role {
        name: String,
        owner: Int@user_id,
        users: &List<User>,
    }
    rel Role.users <<->> User.roles via user_roles[role_id -> roles.id, user_id -> users.id];
"#;

fn compile(query: &str) -> grove_query::codegen::model::CompiledQuery {
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

fn fixture() -> Runtime {
    let rt = Runtime::open_in_memory().unwrap();
    rt.conn()
        .execute_batch(
            "CREATE TABLE users(id INTEGER, name TEXT, age INTEGER);
             CREATE TABLE roles(id INTEGER, name TEXT, user_id INTEGER);
             CREATE TABLE user_roles(user_id INTEGER, role_id INTEGER);",
        )
        .unwrap();
    for (id, name, age) in [(1, "Ada", 30), (2, "Bob", 40), (3, "Cyd", 50)] {
        rt.conn()
            .execute(
                "INSERT INTO users(id, name, age) VALUES (?, ?, ?)",
                rusqlite::params![id, name, age],
            )
            .unwrap();
    }
    for (id, name, owner) in [(1, "admin", 1), (2, "dev", 1)] {
        rt.conn()
            .execute(
                "INSERT INTO roles(id, name, user_id) VALUES (?, ?, ?)",
                rusqlite::params![id, name, owner],
            )
            .unwrap();
    }
    for (user_id, role_id) in [(1, 1), (1, 2), (2, 2)] {
        rt.conn()
            .execute(
                "INSERT INTO user_roles(user_id, role_id) VALUES (?, ?)",
                rusqlite::params![user_id, role_id],
            )
            .unwrap();
    }
    rt
}

fn value(query: &str) -> Value {
    let mut rt = fixture();
    let compiled = compile(query);
    rt.execute_value(&compiled).unwrap()
}

fn role(name: &str) -> Value {
    Value::Record(vec![
        ("name".to_string(), Value::String(name.to_string())),
        ("user_id".to_string(), Value::Int(1)),
    ])
}

#[test]
fn shadowed_pairs_per_parent() {
    assert_eq!(
        value("users { r = roles }"),
        Value::List(vec![
            Value::Record(vec![(
                "r".to_string(),
                Value::List(vec![role("admin"), role("dev")])
            )]),
            Value::Record(vec![("r".to_string(), Value::List(vec![role("dev")]))]),
            Value::Record(vec![("r".to_string(), Value::List(vec![]))]),
        ])
    );
}

#[test]
fn shadowed_scalar_and_values() {
    assert_eq!(
        value("users { a = roles.first()?.name }"),
        Value::List(vec![
            Value::Record(vec![("a".to_string(), Value::String("admin".to_string()))]),
            Value::Record(vec![("a".to_string(), Value::String("dev".to_string()))]),
            Value::Record(vec![("a".to_string(), Value::Null)]),
        ])
    );
    assert_eq!(
        value("users { r = roles { o = owner } }"),
        Value::List(vec![
            Value::Record(vec![(
                "r".to_string(),
                Value::List(vec![
                    Value::Record(vec![("o".to_string(), Value::Int(1))]),
                    Value::Record(vec![("o".to_string(), Value::Int(1))]),
                ])
            )]),
            Value::Record(vec![(
                "r".to_string(),
                Value::List(vec![Value::Record(vec![("o".to_string(), Value::Int(1))])])
            )]),
            Value::Record(vec![("r".to_string(), Value::List(vec![]))]),
        ])
    );
    assert_eq!(
        value("users { n = roles.len() }"),
        Value::List(vec![
            Value::Record(vec![("n".to_string(), Value::Int(2))]),
            Value::Record(vec![("n".to_string(), Value::Int(1))]),
            Value::Record(vec![("n".to_string(), Value::Int(0))]),
        ])
    );
}

#[test]
fn shadowed_take_skip_values() {
    assert_eq!(
        value("users { r = roles.take(1) }"),
        Value::List(vec![
            Value::Record(vec![("r".to_string(), Value::List(vec![role("admin")]))]),
            Value::Record(vec![("r".to_string(), Value::List(vec![role("dev")]))]),
            Value::Record(vec![("r".to_string(), Value::List(vec![]))]),
        ])
    );
    assert_eq!(
        value("users { r = roles.skip(1) }"),
        Value::List(vec![
            Value::Record(vec![("r".to_string(), Value::List(vec![role("dev")]))]),
            Value::Record(vec![("r".to_string(), Value::List(vec![]))]),
            Value::Record(vec![("r".to_string(), Value::List(vec![]))]),
        ])
    );
    assert_eq!(
        value("users { r = roles.take(1) { name } }"),
        Value::List(vec![
            Value::Record(vec![(
                "r".to_string(),
                Value::List(vec![Value::Record(vec![(
                    "name".to_string(),
                    Value::String("admin".to_string())
                )])])
            )]),
            Value::Record(vec![(
                "r".to_string(),
                Value::List(vec![Value::Record(vec![(
                    "name".to_string(),
                    Value::String("dev".to_string())
                )])])
            )]),
            Value::Record(vec![("r".to_string(), Value::List(vec![]))]),
        ])
    );
}

#[test]
fn shadowed_collapse_values() {
    assert_eq!(
        value("users { r = roles.first() }"),
        Value::List(vec![
            Value::Record(vec![("r".to_string(), role("admin"))]),
            Value::Record(vec![("r".to_string(), role("dev"))]),
            Value::Record(vec![("r".to_string(), Value::Null)]),
        ])
    );
    assert_eq!(
        value("users { r = roles.nth(1) }"),
        Value::List(vec![
            Value::Record(vec![("r".to_string(), role("dev"))]),
            Value::Record(vec![("r".to_string(), Value::Null)]),
            Value::Record(vec![("r".to_string(), Value::Null)]),
        ])
    );
    assert_eq!(
        value("users { r = roles.max_by(name) }"),
        Value::List(vec![
            Value::Record(vec![("r".to_string(), role("dev"))]),
            Value::Record(vec![("r".to_string(), role("dev"))]),
            Value::Record(vec![("r".to_string(), Value::Null)]),
        ])
    );
}

#[test]
fn shadowed_nested_scalar_tail_values() {
    let n = |v: &str| Value::String(v.to_string());
    assert_eq!(
        value("users { r = roles.users.first()?.name }"),
        Value::List(vec![
            Value::Record(vec![("r".to_string(), Value::List(vec![n("Ada")]))]),
            Value::Record(vec![(
                "r".to_string(),
                Value::List(vec![n("Ada"), n("Bob")])
            )]),
            Value::Record(vec![("r".to_string(), Value::Null)]),
        ])
    );
}

#[test]
#[should_panic(expected = "M4 many-to-many navigation")]
fn shadowed_scope_take_stays_loud() {
    compile("users { r = roles.take(age) }");
}
