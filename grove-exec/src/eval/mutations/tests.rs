use super::links::link_targets;
use super::records::record_parts;
use super::update::resolve_update_parts;
use super::*;
use crate::eval::harness::*;
use crate::eval::{ExecOptions, exec_file_with_options};
use crate::sqlite::SqliteBackend;
use crate::storage::{FetchConstraints, LinkMode, Storage};
use crate::value::{Key, RecordVal, RowOrigin, Scalar, Val};

use chrono::Utc;
use grove_query::TypedQueryFile;
use grove_schema::validated::{FieldId, StructId, ValidatedSchema};
use rusqlite::Connection;
use std::rc::Rc;

fn test_schema() -> ValidatedSchema {
    grove_schema::validate(
        grove_schema::parse_schema(
            r#"
                root items: Item;
                enum Kind { A => 1, B => 2 }
                struct Item {
                    id: Int,
                    name: String,
                    active: Bool,
                    note: ?String,
                    pos: (Float, Float)@(x, y),
                    kind: Kind,
                    maybe_kind: ?Kind,
                }
                "#,
        )
        .0
        .expect("invalid mutation test schema"),
    )
    .0
    .expect("invalid mutation test schema")
}

fn test_backend(schema: &ValidatedSchema) -> SqliteBackend<'_> {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE items(id INTEGER, name TEXT, active INTEGER, note TEXT,
             x REAL, y REAL, kind INTEGER, maybe_kind INTEGER)",
    )
    .unwrap();
    SqliteBackend::new(conn, schema).unwrap()
}

fn typed(source: &str, schema: &ValidatedSchema) -> TypedQueryFile {
    let (file, parse_diags) = grove_query::parse_query(source);
    assert!(parse_diags.is_empty(), "{parse_diags:?}");
    let (typed, diags) = grove_query::typecheck::typecheck(file.unwrap(), schema);
    assert!(diags.is_empty(), "{diags:?}");
    typed.unwrap()
}

fn run(source: &str, schema: &ValidatedSchema, backend: &SqliteBackend<'_>) -> Option<Val> {
    run_result(source, schema, backend).unwrap()
}

fn run_result(
    source: &str,
    schema: &ValidatedSchema,
    backend: &SqliteBackend<'_>,
) -> Result<Option<Val>, ExecError> {
    let pushed = run_result_with_pushdown(source, schema, backend, true, true);
    let unpushed = run_result_with_pushdown(source, schema, backend, false, false);
    assert_pushdown_parity(source, &pushed, &unpushed);
    unpushed
}

fn run_result_with_pushdown(
    source: &str,
    schema: &ValidatedSchema,
    backend: &SqliteBackend<'_>,
    pushdown: bool,
    debug_rollback: bool,
) -> Result<Option<Val>, ExecError> {
    let file = typed(source, schema);
    exec_file_with_options(
        &file,
        backend,
        schema,
        Utc::now(),
        ExecOptions {
            pushdown,
            debug_rollback,
            ..ExecOptions::default()
        },
    )
}

fn fields(schema: &ValidatedSchema) -> (StructId, Vec<FieldId>) {
    let item = schema.roots[0].struct_id;
    let fields = ["id", "name", "active", "note", "pos", "kind", "maybe_kind"]
        .iter()
        .map(|name| schema.struct_field(item, name).unwrap().0)
        .collect();
    (item, fields)
}

#[test]
fn insert_root_record_scalar_enum_tuple_optional() {
    let schema = test_schema();
    let backend = test_backend(&schema);
    let (item, fields) = fields(&schema);
    let kind = schema.enum_by_name("Kind").unwrap().0;

    run(
        r#"items.insert({
                id = 1,
                name = "alpha",
                active = true,
                note = some("hello"),
                pos = (1.5f, -2.5f),
                kind = Kind::B,
                maybe_kind = some(Kind::A)
            });"#,
        &schema,
        &backend,
    );

    let rows = backend
        .fetch(item, &fields, &FetchConstraints::EMPTY)
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].values,
        vec![
            int(1),
            str("alpha"),
            bool(true),
            some(str("hello")),
            Val::Tuple(vec![float(1.5), float(-2.5),]),
            Val::Enum(kind, 1),
            some(Val::Enum(kind, 0)),
        ]
    );
}

#[test]
fn insert_root_bulk_list() {
    let schema = test_schema();
    let backend = test_backend(&schema);
    let (item, fields) = fields(&schema);

    run(
        r#"items.insert([
                { id = 1, name = "alpha", active = true, pos = (1.0f, 2.0f), kind = Kind::A },
                { id = 2, name = "beta", active = false, pos = (3.0f, 4.0f), kind = Kind::B },
            ]);"#,
        &schema,
        &backend,
    );

    let rows = backend
        .fetch(item, &fields, &FetchConstraints::EMPTY)
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].values[0], Val::Scalar(Scalar::Int(1)));
    assert_eq!(rows[1].values[0], Val::Scalar(Scalar::Int(2)));
    assert_eq!(rows[0].values[6], Val::None);
    assert_eq!(
        rows[1].values[1],
        Val::Scalar(Scalar::String("beta".to_string()))
    );
}

#[test]
fn insert_filtered_root_and_same_turn_read() {
    let schema = test_schema();
    let backend = test_backend(&schema);

    let result = run(
        r#"items[id > 0].insert({
                id = 7,
                name = "gamma",
                active = false,
                pos = (0.0f, 0.0f),
                kind = Kind::A
            }); items.len()"#,
        &schema,
        &backend,
    );

    assert_eq!(result, Some(Val::Scalar(Scalar::Int(1))));
}

fn ref_schema() -> ValidatedSchema {
    grove_schema::validate(
        grove_schema::parse_schema(
            r#"
                root users: User;
                struct User {
                    id: Int,
                    name: String,
                    orders: List<Order>,
                    profile: ?Profile,
                }
                struct Order {
                    id: Int,
                    total: Int,
                    user: &User,
                }
                struct Profile {
                    bio: String,
                    user: &User,
                }
                rel Order.user <<-> User.orders (orders.user_id -> users.id);
                rel Profile.user <-> User.profile (profiles.user_id -> users.id);
                "#,
        )
        .0
        .expect("invalid ref mutation schema"),
    )
    .0
    .expect("invalid ref mutation schema")
}

fn ref_backend(schema: &ValidatedSchema) -> SqliteBackend<'_> {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE users(id INTEGER, name TEXT);
             CREATE TABLE orders(id INTEGER, total INTEGER, user_id INTEGER);
             CREATE TABLE profiles(bio TEXT, user_id INTEGER);",
    )
    .unwrap();
    SqliteBackend::new(conn, schema).unwrap()
}

#[test]
fn insert_nested_owning_collection_target() {
    let schema = ref_schema();
    let backend = ref_backend(&schema);

    let result = run(
        r#"
            users.insert({ id = 1, name = "A", orders = [] });
            users.first().unwrap().orders.insert({ id = 10, total = 100 });
            users.first().unwrap().orders { id, total }
            "#,
        &schema,
        &backend,
    );

    assert_eq!(
        result,
        Some(Val::list(vec![rec(vec![
            ("id", int(10)),
            ("total", int(100)),
        ])]))
    );
}

#[test]
fn insert_recursive_owning_children() {
    let schema = ref_schema();
    let backend = ref_backend(&schema);

    let result = run(
        r#"
            users.insert({
                id = 1,
                name = "A",
                orders = [
                    { id = 10, total = 100 },
                    { id = 11, total = 110 },
                ],
            });
            users.first().unwrap().orders { id, total }
            "#,
        &schema,
        &backend,
    );

    assert_eq!(
        result,
        Some(Val::list(vec![
            rec(vec![("id", int(10)), ("total", int(100))]),
            rec(vec![("id", int(11)), ("total", int(110))]),
        ]))
    );
}

#[test]
fn insert_optional_single_owning_child() {
    let schema = ref_schema();
    let backend = ref_backend(&schema);

    let result = run(
        r#"
            users.insert({
                id = 1,
                name = "A",
                orders = [],
                profile = some({ bio = "bio" }),
            });
            users.first().unwrap().profile?.bio
            "#,
        &schema,
        &backend,
    );

    assert_eq!(result, Some(some(str("bio"))));
}

#[test]
fn insert_bulk_recursive_children_per_parent() {
    let schema = ref_schema();
    let backend = ref_backend(&schema);

    let result = run(
        r#"
            users.insert([
                { id = 1, name = "A", orders = [{ id = 10, total = 1 }] },
                {
                    id = 2,
                    name = "B",
                    orders = [
                        { id = 20, total = 2 },
                        { id = 21, total = 3 },
                    ],
                },
                { id = 3, name = "C", orders = [] },
            ]);
            users.map($.orders.len())
            "#,
        &schema,
        &backend,
    );

    assert_eq!(result, Some(Val::list(vec![int(1), int(2), int(0)])));
}

fn array_schema() -> ValidatedSchema {
    grove_schema::validate(
        grove_schema::parse_schema(
            r#"
                root rows: Row;
                struct Row {
                    id: Int,
                    tags: List<String>@tags_ref via row_tags[row_key, tag],
                    nums: List<Int>@nums_ref via row_nums[row_key, num],
                    matrix: List<List<Int> via inner_nums[inner_key, val]>@matrix_ref
                        via outer_nums[outer_key, inner_key],
                    extra: (Int, List<String> via extra_tags[ekey, eval])@(eid, elink),
                }
                "#,
        )
        .0
        .expect("invalid array mutation schema"),
    )
    .0
    .expect("invalid array mutation schema")
}

fn array_backend(schema: &ValidatedSchema) -> SqliteBackend<'_> {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE rows(id INTEGER, tags_ref INTEGER, nums_ref INTEGER,
                 matrix_ref INTEGER, eid INTEGER, elink INTEGER);
             CREATE TABLE row_tags(row_key INTEGER, tag TEXT);
             CREATE TABLE row_nums(row_key INTEGER, num INTEGER);
             CREATE TABLE outer_nums(outer_key INTEGER, inner_key INTEGER);
             CREATE TABLE inner_nums(inner_key INTEGER, val INTEGER);
             CREATE TABLE extra_tags(ekey INTEGER, eval TEXT);",
    )
    .unwrap();
    SqliteBackend::new(conn, schema).unwrap()
}

#[test]
fn insert_array_fields_and_nested_value_arrays() {
    let schema = array_schema();
    let backend = array_backend(&schema);

    let result = run(
        r#"
            rows.insert({
                id = 1,
                tags = ["a", "b"],
                nums = [1, 2, 3],
                matrix = [[1, 2], [3]],
                extra = (7, ["x", "y"]),
            });
            rows.first().unwrap() { tags, nums, matrix, extra }
            "#,
        &schema,
        &backend,
    );

    assert_eq!(
        result,
        Some(rec(vec![
            ("tags", Val::list(vec![str("a"), str("b")]),),
            ("nums", Val::list(vec![int(1), int(2), int(3)])),
            (
                "matrix",
                Val::list(vec![
                    Val::list(vec![int(1), int(2)]),
                    Val::list(vec![int(3)]),
                ]),
            ),
            (
                "extra",
                Val::Tuple(vec![int(7), Val::list(vec![str("x"), str("y")])]),
            ),
        ]))
    );
}

#[test]
fn insert_empty_and_multiple_array_rows() {
    let schema = array_schema();
    let backend = array_backend(&schema);

    let result = run(
        r#"
            rows.insert([
                { id = 1, tags = [], nums = [1], matrix = [], extra = (1, []) },
                { id = 2, tags = ["z"], nums = [2, 3], matrix = [[4]], extra = (2, ["q"]) },
            ]);
            rows { id, tags, nums, matrix, extra }
            "#,
        &schema,
        &backend,
    );

    let Some(Val::List(rows)) = result else {
        panic!("expected rows list");
    };
    assert_eq!(rows.len(), 2);
    assert_eq!(
        rows[0],
        rec(vec![
            ("id", int(1)),
            ("tags", Val::list(vec![])),
            ("nums", Val::list(vec![int(1)])),
            ("matrix", Val::list(vec![])),
            ("extra", Val::Tuple(vec![int(1), Val::list(vec![])])),
        ])
    );
    assert_eq!(
        rows[1],
        rec(vec![
            ("id", int(2)),
            ("tags", Val::list(vec![str("z")])),
            ("nums", Val::list(vec![int(2), int(3)])),
            ("matrix", Val::list(vec![Val::list(vec![int(4)])])),
            ("extra", Val::Tuple(vec![int(2), Val::list(vec![str("q")])]),),
        ])
    );
}

#[test]
fn insert_optional_none_target_noop() {
    let schema = ref_schema();
    let backend = ref_backend(&schema);

    let result = run(
        r#"
            users.first()?.orders?.insert({ id = 1, total = 1 });
            users.len()
            "#,
        &schema,
        &backend,
    );

    assert_eq!(result, Some(int(0)));
}

#[test]
fn insert_nested_target_missing_parent_key_error() {
    let schema = ref_schema();
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE users(id INTEGER, name TEXT);
             CREATE TABLE orders(id INTEGER, total INTEGER, user_id INTEGER);
             CREATE TABLE profiles(bio TEXT, user_id INTEGER);
             INSERT INTO users(id, name) VALUES (NULL, 'A');",
    )
    .unwrap();
    let backend = SqliteBackend::new(conn, &schema).unwrap();

    let file = typed(
        "users.first()?.orders?.insert({ id = 1, total = 1 });",
        &schema,
    );
    let err = exec_file_with_options(&file, &backend, &schema, Utc::now(), ExecOptions::default())
        .unwrap_err();
    let ExecError::Integrity { message } = err else {
        panic!("expected integrity error, got {err:?}");
    };
    assert!(message.contains("no join key"), "{message}");
}

fn cascade_schema() -> ValidatedSchema {
    grove_schema::validate(
        grove_schema::parse_schema(
            r#"
                root users: User;
                struct User {
                    id: Int,
                    name: String,
                    orders: List<Order>,
                }
                struct Order {
                    id: Int,
                    total: Int,
                    user: &User,
                    items: List<Item>,
                }
                struct Item {
                    id: Int,
                    order: &Order,
                }
                rel Order.user <<-> User.orders (orders.user_id -> users.id);
                rel Item.order <<-> Order.items (items.order_id -> orders.id);
                "#,
        )
        .0
        .expect("invalid cascade schema"),
    )
    .0
    .expect("invalid cascade schema")
}

fn cascade_backend(schema: &ValidatedSchema) -> SqliteBackend<'_> {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE users(id INTEGER, name TEXT);
             CREATE TABLE orders(id INTEGER, total INTEGER, user_id INTEGER);
             CREATE TABLE items(id INTEGER, order_id INTEGER);",
    )
    .unwrap();
    SqliteBackend::new(conn, schema).unwrap()
}

#[test]
fn delete_list_target_recursive_cascade() {
    let schema = cascade_schema();
    let backend = cascade_backend(&schema);
    let user = schema.roots[0].struct_id;
    let order = schema.struct_by_name("Order").unwrap().0;
    let item = schema.struct_by_name("Item").unwrap().0;

    let result = run(
        r#"
            users.insert([
                {
                    id = 1,
                    name = "A",
                    orders = [{
                        id = 10,
                        total = 1,
                        items = [{ id = 100 }, { id = 101 }],
                    }],
                },
                {
                    id = 2,
                    name = "B",
                    orders = [{ id = 20, total = 2, items = [] }],
                },
            ]);
            users[id == 1].delete();
            users.len()
            "#,
        &schema,
        &backend,
    );

    assert_eq!(result, Some(int(1)));
    assert_eq!(
        backend
            .fetch(user, &[], &FetchConstraints::EMPTY)
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        backend
            .fetch(order, &[], &FetchConstraints::EMPTY)
            .unwrap()
            .len(),
        1
    );
    assert!(
        backend
            .fetch(item, &[], &FetchConstraints::EMPTY)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn delete_record_target() {
    let schema = cascade_schema();
    let backend = cascade_backend(&schema);
    let user = schema.roots[0].struct_id;
    let order = schema.struct_by_name("Order").unwrap().0;

    let result = run(
        r#"
            users.insert({
                id = 1,
                name = "A",
                orders = [{ id = 10, total = 1, items = [] }],
            });
            users.find(name == "A").unwrap().delete();
            users.len()
            "#,
        &schema,
        &backend,
    );

    assert_eq!(result, Some(int(0)));
    assert!(
        backend
            .fetch(user, &[], &FetchConstraints::EMPTY)
            .unwrap()
            .is_empty()
    );
    assert!(
        backend
            .fetch(order, &[], &FetchConstraints::EMPTY)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn delete_duplicate_targets_deduped() {
    let schema = cascade_schema();
    let backend = cascade_backend(&schema);
    let user = schema.roots[0].struct_id;

    let result = run(
        r#"
            users.insert({
                id = 1,
                name = "A",
                orders = [{ id = 10, total = 1, items = [{ id = 100 }] }],
            });
            users.extend(users).delete();
            users.len()
            "#,
        &schema,
        &backend,
    );

    assert_eq!(result, Some(int(0)));
    assert!(
        backend
            .fetch(user, &[], &FetchConstraints::EMPTY)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn delete_optional_none_target_noop() {
    let schema = cascade_schema();
    let backend = cascade_backend(&schema);

    let result = run(
        r#"
            users.first()?.delete();
            users.len()
            "#,
        &schema,
        &backend,
    );

    assert_eq!(result, Some(int(0)));
}

fn one_to_many_schema() -> ValidatedSchema {
    grove_schema::validate(
        grove_schema::parse_schema(
            r#"
                root parents: Parent;
                root children: Child;
                struct Parent {
                    id: Int,
                    name: String,
                    children: &List<Child>,
                }
                struct Child {
                    id: Int,
                    name: String,
                    parent: &?Parent,
                }
                rel Parent.children <->> Child.parent (children.parent_id -> parents.id);
                "#,
        )
        .0
        .expect("invalid one-to-many schema"),
    )
    .0
    .expect("invalid one-to-many schema")
}

fn one_to_many_backend(schema: &ValidatedSchema) -> SqliteBackend<'_> {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE parents(id INTEGER, name TEXT);
             CREATE TABLE children(id INTEGER, name TEXT, parent_id INTEGER);
             INSERT INTO parents(id, name) VALUES (1, 'parent');
             INSERT INTO children(id, name, parent_id) VALUES (10, 'child', 1);",
    )
    .unwrap();
    SqliteBackend::new(conn, schema).unwrap()
}

#[test]
fn delete_one_to_many_fk_cleared() {
    let schema = one_to_many_schema();
    let backend = one_to_many_backend(&schema);

    let result = run(
        r#"
            parents.first().unwrap().delete();
            children.first().unwrap() { id, parent }
            "#,
        &schema,
        &backend,
    );

    assert_eq!(
        result,
        Some(rec(vec![("id", int(10)), ("parent", Val::None)]))
    );
}

fn self_ref_schema() -> ValidatedSchema {
    grove_schema::validate(
        grove_schema::parse_schema(
            r#"
                root users: User;
                struct User {
                    id: Int,
                    name: String,
                    manager: &?User,
                    subordinates: &List<User>,
                }
                rel User.manager <<-> User.subordinates (users.manager_id -> users.id);
                "#,
        )
        .0
        .expect("invalid self-ref schema"),
    )
    .0
    .expect("invalid self-ref schema")
}

fn self_ref_backend(schema: &ValidatedSchema) -> SqliteBackend<'_> {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE users(id INTEGER, name TEXT, manager_id INTEGER);
             INSERT INTO users(id, name, manager_id) VALUES
                 (1, 'manager', NULL),
                 (2, 'report', 1);",
    )
    .unwrap();
    SqliteBackend::new(conn, schema).unwrap()
}

#[test]
fn delete_self_ref_fk_cleared() {
    let schema = self_ref_schema();
    let backend = self_ref_backend(&schema);

    let result = run(
        r#"
            users.first().unwrap().delete();
            users.first().unwrap() { id, manager }
            "#,
        &schema,
        &backend,
    );

    assert_eq!(
        result,
        Some(rec(vec![("id", int(2)), ("manager", Val::None)]))
    );
}

fn many_to_many_schema() -> ValidatedSchema {
    grove_schema::validate(
        grove_schema::parse_schema(
            r#"
                root users: User;
                root roles: Role;
                struct User {
                    id: Int,
                    name: String,
                    roles: &List<Role>,
                }
                struct Role {
                    id: Int,
                    name: String,
                    users: &List<User>,
                }
                rel Role.users <<->> User.roles
                    via user_roles[role_id -> roles.id, user_id -> users.id];
                "#,
        )
        .0
        .expect("invalid many-to-many schema"),
    )
    .0
    .expect("invalid many-to-many schema")
}

fn many_to_many_backend(schema: &ValidatedSchema) -> SqliteBackend<'_> {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE users(id INTEGER, name TEXT);
             CREATE TABLE roles(id INTEGER, name TEXT);
             CREATE TABLE user_roles(role_id INTEGER, user_id INTEGER);
             INSERT INTO users(id, name) VALUES (1, 'u1'), (2, 'u2');
             INSERT INTO roles(id, name) VALUES (1, 'r1'), (2, 'r2');
             INSERT INTO user_roles(role_id, user_id) VALUES (1, 1), (1, 2), (2, 1);",
    )
    .unwrap();
    SqliteBackend::new(conn, schema).unwrap()
}

fn nested_link_schema() -> ValidatedSchema {
    grove_schema::validate(
        grove_schema::parse_schema(
            r#"
                root users: User;
                root images: Image;
                root tags: Tag;
                struct User {
                    id: Int,
                    name: String,
                    favorite: &?Image,
                    orders: List<Order>,
                }
                struct Order {
                    id: Int,
                    total: Int,
                    user: &User,
                    image: &?Image,
                    tags: &List<Tag>,
                }
                struct Image {
                    id: Int,
                    title: String,
                    users: &List<User>,
                    orders: &List<Order>,
                }
                struct Tag {
                    id: Int,
                    label: String,
                    orders: &List<Order>,
                }
                rel Order.user <<-> User.orders (orders.user_id -> users.id);
                rel Image.users <->> User.favorite (users.favorite_id -> images.id);
                rel Image.orders <->> Order.image (orders.image_id -> images.id);
                rel Tag.orders <<->> Order.tags
                    via order_tags[tag_id -> tags.id, order_id -> orders.id];
                "#,
        )
        .0
        .expect("invalid nested-link schema"),
    )
    .0
    .expect("invalid nested-link schema")
}

fn nested_link_backend(schema: &ValidatedSchema) -> SqliteBackend<'_> {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE users(id INTEGER, name TEXT, favorite_id INTEGER);
             CREATE TABLE images(id INTEGER, title TEXT);
             CREATE TABLE tags(id INTEGER, label TEXT);
             CREATE TABLE orders(id INTEGER, total INTEGER, user_id INTEGER, image_id INTEGER);
             CREATE TABLE order_tags(tag_id INTEGER, order_id INTEGER);
             INSERT INTO images(id, title) VALUES (1, 'i1'), (2, 'i2');
             INSERT INTO tags(id, label) VALUES (1, 't1'), (2, 't2');",
    )
    .unwrap();
    SqliteBackend::new(conn, schema).unwrap()
}

fn mutation_parts_schema() -> ValidatedSchema {
    grove_schema::validate(
        grove_schema::parse_schema(
            r#"
                root users: User;
                root tags: Tag;
                struct User {
                    id: Int,
                    name: String,
                    manager: &?User,
                    subordinates: &List<User>,
                    tags: &List<Tag>,
                    orders: List<Order>,
                }
                struct Order {
                    id: Int,
                    total: Int,
                    user: &User,
                }
                struct Tag {
                    id: Int,
                    label: String,
                    users: &List<User>,
                }
                rel User.manager <<-> User.subordinates (users.manager_id -> users.id);
                rel Order.user <<-> User.orders (orders.user_id -> users.id);
                rel Tag.users <<->> User.tags
                    via user_tags[tag_id -> tags.id, user_id -> users.id];
                "#,
        )
        .0
        .expect("invalid split mutation schema"),
    )
    .0
    .expect("invalid split mutation schema")
}

fn mutation_parts_ids(schema: &ValidatedSchema) -> (StructId, StructId, StructId) {
    (
        schema.struct_by_name("User").unwrap().0,
        schema.struct_by_name("Order").unwrap().0,
        schema.struct_by_name("Tag").unwrap().0,
    )
}

#[test]
fn insert_parts_split_base_owned_links() {
    let schema = mutation_parts_schema();
    let (user, _, tag) = mutation_parts_ids(&schema);
    let manager = RowOrigin {
        struct_id: user,
        key: Key(99),
    };
    let tag_link = RowOrigin {
        struct_id: tag,
        key: Key(7),
    };
    let manager_field = schema.struct_field(user, "manager").unwrap().0;
    let tags_field = schema.struct_field(user, "tags").unwrap().0;
    let orders_field = schema.struct_field(user, "orders").unwrap().0;
    let id_field = schema.struct_field(user, "id").unwrap().0;
    let name_field = schema.struct_field(user, "name").unwrap().0;

    let record = RecordVal::new(vec![
        (Rc::from("id"), int(1)),
        (Rc::from("name"), str("A")),
        (Rc::from("manager"), some(Val::RowRef(manager))),
        (Rc::from("tags"), Val::list(vec![Val::RowRef(tag_link)])),
        (
            Rc::from("orders"),
            Val::list(vec![rec(vec![("id", int(10)), ("total", int(100))])]),
        ),
    ]);
    let parts = record_parts(&schema, user, record).unwrap();

    assert_eq!(parts.base, vec![(id_field, int(1)), (name_field, str("A"))]);
    assert_eq!(parts.owned.len(), 1);
    assert_eq!(parts.owned[0].0, orders_field);
    assert!(matches!(parts.owned[0].1, Val::List(_)));
    assert_eq!(
        parts.links,
        vec![(manager_field, vec![manager]), (tags_field, vec![tag_link]),]
    );
}

#[test]
fn update_parts_split_base_owned_links() {
    let schema = mutation_parts_schema();
    let (user, _, tag) = mutation_parts_ids(&schema);
    let tag_link = RowOrigin {
        struct_id: tag,
        key: Key(7),
    };
    let manager_field = schema.struct_field(user, "manager").unwrap().0;
    let tags_field = schema.struct_field(user, "tags").unwrap().0;
    let orders_field = schema.struct_field(user, "orders").unwrap().0;
    let name_field = schema.struct_field(user, "name").unwrap().0;

    let record = RecordVal::new(vec![
        (Rc::from("name"), str("B")),
        (Rc::from("manager"), Val::None),
        (Rc::from("tags"), Val::list(vec![Val::RowRef(tag_link)])),
        (
            Rc::from("orders"),
            Val::list(vec![rec(vec![("id", int(10)), ("total", int(100))])]),
        ),
    ]);
    let parts = resolve_update_parts(&schema, user, record).unwrap();

    assert_eq!(parts.base, vec![(name_field, str("B"))]);
    assert!(parts.arrays.is_empty());
    assert_eq!(parts.owned.len(), 1);
    assert_eq!(parts.owned[0].0, orders_field);
    assert_eq!(
        parts.links,
        vec![(manager_field, Vec::new()), (tags_field, vec![tag_link])]
    );
}

#[test]
fn link_split_mismatched_target_rejected() {
    let schema = mutation_parts_schema();
    let (user, order, _) = mutation_parts_ids(&schema);
    let wrong = RowOrigin {
        struct_id: order,
        key: Key(1),
    };
    let record = RecordVal::new(vec![(Rc::from("manager"), some(Val::RowRef(wrong)))]);
    assert!(matches!(
        resolve_update_parts(&schema, user, record),
        Err(ExecError::Internal { .. })
    ));
}

#[test]
fn update_non_owning_link() {
    let schema = one_to_many_schema();
    let backend = one_to_many_backend(&schema);

    let result = run(
        r#"
            children.insert({ id = 11, name = "child2", parent = none });
            children[id == 11].update({ parent = some(&parents.first().unwrap()) });
            children[id == 11].first().unwrap().parent?.id
            "#,
        &schema,
        &backend,
    );
    assert_eq!(result, Some(some(int(1))));

    let result = run(
        r#"
            children[id == 11].update({ parent = none });
            children[id == 11].first().unwrap().parent?.id
            "#,
        &schema,
        &backend,
    );
    assert_eq!(result, Some(Val::None));
}

#[test]
fn insert_non_owning_link_nested() {
    let schema = one_to_many_schema();
    let backend = one_to_many_backend(&schema);

    let result = run(
        r#"
            children.insert({
                id = 11,
                name = "child2",
                parent = some(&parents.first().unwrap()),
            });
            children.find(id == 11).unwrap().parent?.id
            "#,
        &schema,
        &backend,
    );
    assert_eq!(result, Some(some(int(1))));
}

#[test]
fn insert_non_owning_list() {
    let schema = one_to_many_schema();
    let backend = one_to_many_backend(&schema);

    let result = run(
        r#"
            children.insert({ id = 11, name = "child2", parent = none });
            parents.first().unwrap().children.insert(&children[id == 11]);
            (children.len(), parents.first().unwrap().children.len())
            "#,
        &schema,
        &backend,
    );
    assert_eq!(result, Some(Val::Tuple(vec![int(2), int(2)])));
}

#[test]
fn update_non_owning_list() {
    let schema = one_to_many_schema();
    let backend = one_to_many_backend(&schema);

    let result = run(
        r#"
            children.insert({ id = 11, name = "child2", parent = none });
            parents.first().unwrap().update({ children = &children });
            children.map($.parent?.id)
            "#,
        &schema,
        &backend,
    );
    assert_eq!(result, Some(Val::list(vec![some(int(1)), some(int(1))])));

    let result = run(
        r#"
            parents.first().unwrap().update({ children = [&children.find(id == 11).unwrap()] });
            (children.len(), children.map($.parent?.id))
            "#,
        &schema,
        &backend,
    );
    assert_eq!(
        result,
        Some(Val::Tuple(vec![
            int(2),
            Val::list(vec![Val::None, some(int(1))]),
        ]))
    );
}

#[test]
fn link_targets_deduplication() {
    let schema = one_to_many_schema();
    let parent = schema.struct_by_name("Parent").unwrap().0;
    let child = schema.struct_by_name("Child").unwrap().0;
    let field = schema.struct_field(parent, "children").unwrap().0;
    let first = RowOrigin {
        struct_id: child,
        key: Key(10),
    };
    let second = RowOrigin {
        struct_id: child,
        key: Key(11),
    };
    let targets = link_targets(
        &schema,
        field,
        Val::list(vec![
            Val::RowRef(first),
            Val::RowRef(second),
            Val::RowRef(first),
        ]),
    )
    .unwrap();
    assert_eq!(targets, vec![first, second]);
}

#[test]
fn self_ref_non_owning_mutations() {
    let schema = self_ref_schema();
    let backend = self_ref_backend(&schema);

    let result = run(
        r#"
            users.insert({ id = 3, name = "third", manager = none });
            users[id == 1].update({ subordinates = &users[id == 2] });
            (
                users.find(id == 1).unwrap().subordinates.len(),
                users.find(id == 3).unwrap().manager?.id
            )
            "#,
        &schema,
        &backend,
    );
    assert_eq!(result, Some(Val::Tuple(vec![int(1), Val::None])));

    let result = run(
        r#"
            users.find(id == 1).unwrap().subordinates.insert(&users[id == 3]);
            (
                users[id == 1].first().unwrap().subordinates.len(),
                users[id == 3].first().unwrap().manager?.id
            )
            "#,
        &schema,
        &backend,
    );
    assert_eq!(result, Some(Val::Tuple(vec![int(2), some(int(1))])));

    let result = run(
        r#"
            users.find(id == 3)?.update({ manager = some(&users.find(id == 1).unwrap()) });
            users.find(id == 3)?.manager??.id
            "#,
        &schema,
        &backend,
    );
    assert_eq!(result, Some(some(some(int(1)))));

    let result = run(
        r#"
            users[id == 3].update({ manager = none });
            users.find(id == 3).unwrap().manager?.id
            "#,
        &schema,
        &backend,
    );
    assert_eq!(result, Some(Val::None));
}

#[test]
fn link_api_missing_targets_rejected() {
    let schema = one_to_many_schema();
    let backend = one_to_many_backend(&schema);
    let parent = schema.struct_by_name("Parent").unwrap().0;
    let child = schema.struct_by_name("Child").unwrap().0;
    let parent_field = schema.struct_field(child, "parent").unwrap().0;
    let children_field = schema.struct_field(parent, "children").unwrap().0;
    let missing_parent = RowOrigin {
        struct_id: parent,
        key: Key(999),
    };
    let missing_child = RowOrigin {
        struct_id: child,
        key: Key(999),
    };

    let mut txn = backend.begin_write().unwrap();
    assert!(matches!(
        txn.link_row_refs(parent_field, Key(1), &[missing_parent], LinkMode::Append),
        Err(ExecError::Integrity { .. })
    ));
    assert!(matches!(
        txn.link_row_refs(children_field, Key(1), &[missing_child], LinkMode::Append),
        Err(ExecError::Integrity { .. })
    ));
    txn.rollback().unwrap();
}

#[test]
fn many_to_many_link_missing_targets_rejected() {
    let schema = many_to_many_schema();
    let backend = many_to_many_backend(&schema);
    let user = schema.struct_by_name("User").unwrap().0;
    let role = schema.struct_by_name("Role").unwrap().0;
    let roles_field = schema.struct_field(user, "roles").unwrap().0;
    let missing = RowOrigin {
        struct_id: role,
        key: Key(999),
    };

    let mut txn = backend.begin_write().unwrap();
    assert!(matches!(
        txn.link_row_refs(roles_field, Key(1), &[missing], LinkMode::Append),
        Err(ExecError::Integrity { .. })
    ));
    txn.rollback().unwrap();
}

#[test]
fn many_to_many_append_idempotence() {
    let schema = many_to_many_schema();
    let backend = many_to_many_backend(&schema);

    let result = run(
        r#"
            users.find(id == 2).unwrap().roles.insert(&roles[id == 2]);
            users.find(id == 2).unwrap().roles.insert(&roles[id == 2]);
            roles.find(id == 1).unwrap().users.insert(&users[id == 2]);
            (
                users.find(id == 2).unwrap().roles.len(),
                roles.find(id == 1).unwrap().users.len(),
                roles.len()
            )
            "#,
        &schema,
        &backend,
    );
    assert_eq!(result, Some(Val::Tuple(vec![int(2), int(2), int(2)])));
}

#[test]
fn many_to_many_replace_any_endpoint() {
    let schema = many_to_many_schema();
    let backend = many_to_many_backend(&schema);

    let result = run(
        r#"
            users.find(id == 1)?.update({ roles = &roles[id == 2] });
            (
                users.find(id == 1).unwrap().roles.id,
                roles.len()
            )
            "#,
        &schema,
        &backend,
    );
    assert_eq!(
        result,
        Some(Val::Tuple(vec![Val::list(vec![int(2)]), int(2),]))
    );

    let result = run(
        r#"
            roles.find(id == 2)?.update({ users = &users[id == 2] });
            (
                roles.find(id == 2).unwrap().users.id,
                users.find(id == 1).unwrap().roles.len(),
                users.find(id == 2).unwrap().roles.id,
                users.len(),
            )
            "#,
        &schema,
        &backend,
    );
    assert_eq!(
        result,
        Some(Val::Tuple(vec![
            Val::list(vec![int(2)]),
            int(0),
            Val::list(vec![int(1), int(2)]),
            int(2),
        ]))
    );
}

#[test]
fn many_to_many_clear_join_table() {
    let schema = many_to_many_schema();
    let backend = many_to_many_backend(&schema);

    let result = run(
        r#"
            users.find(id == 1)?.update({ roles = [] });
            (
                users.find(id == 1).unwrap().roles.len(),
                roles.len(),
                users.len(),
            )
            "#,
        &schema,
        &backend,
    );
    assert_eq!(result, Some(Val::Tuple(vec![int(0), int(2), int(2)])));
}

#[test]
fn owned_insert_nested_non_owning() {
    let schema = nested_link_schema();
    let backend = nested_link_backend(&schema);

    let result = run(
        r#"
            users.insert({
                id = 1,
                name = "u",
                favorite = some(&images[id == 1].first().unwrap()),
                orders = [{
                    id = 10,
                    total = 1,
                    image = some(&images[id == 1].first().unwrap()),
                    tags = &tags,
                }],
            });
            (
                users.find(id == 1).unwrap().favorite?.id,
                users.find(id == 1).unwrap().orders.first().unwrap().image?.id,
                users.find(id == 1).unwrap().orders.first().unwrap().tags.id,
            )
            "#,
        &schema,
        &backend,
    );
    assert_eq!(
        result,
        Some(Val::Tuple(vec![
            some(int(1)),
            some(int(1)),
            Val::list(vec![int(1), int(2)]),
        ]))
    );
}

#[test]
fn owned_update_nested_non_owning() {
    let schema = nested_link_schema();
    let backend = nested_link_backend(&schema);

    let result = run(
        r#"
            users.insert({
                id = 1,
                name = "u",
                favorite = some(&images[id == 1].first().unwrap()),
                orders = [{
                    id = 10,
                    total = 1,
                    image = some(&images[id == 1].first().unwrap()),
                    tags = &tags,
                }],
            });
            users.find(id == 1).unwrap().update({
                name = "u2",
                favorite = some(&images[id == 2].first().unwrap()),
                orders = [{
                    id = 11,
                    total = 2,
                    image = some(&images[id == 2].first().unwrap()),
                    tags = &tags[id == 1],
                }],
            });
            (
                users.find(id == 1).unwrap().name,
                users.find(id == 1).unwrap().favorite?.id,
                users.find(id == 1).unwrap().orders.id,
                users.find(id == 1).unwrap().orders.first().unwrap().image?.id,
                users.find(id == 1).unwrap().orders.first().unwrap().tags.id,
                images.len(),
            )
            "#,
        &schema,
        &backend,
    );
    assert_eq!(
        result,
        Some(Val::Tuple(vec![
            str("u2"),
            some(int(2)),
            Val::list(vec![int(11)]),
            some(int(2)),
            Val::list(vec![int(1)]),
            int(2),
        ]))
    );
}

#[test]
fn owning_delete_nested_non_owning_cleaned() {
    let schema = nested_link_schema();
    let backend = nested_link_backend(&schema);

    let result = run(
        r#"
            users.insert({
                id = 1,
                name = "u",
                favorite = some(&images[id == 1].first().unwrap()),
                orders = [{
                    id = 10,
                    total = 1,
                    image = some(&images[id == 1].first().unwrap()),
                    tags = &tags,
                }],
            });
            images[id == 1].first().unwrap().delete();
            tags[id == 1].first().unwrap().delete();
            (
                users.find(id == 1).unwrap().favorite?.id,
                users.find(id == 1).unwrap().orders.first().unwrap().image?.id,
                users.find(id == 1).unwrap().orders.first().unwrap().tags.id,
                users.len(),
                images.len(),
            )
            "#,
        &schema,
        &backend,
    );
    assert_eq!(
        result,
        Some(Val::Tuple(vec![
            Val::None,
            Val::None,
            Val::list(vec![int(2)]),
            int(1),
            int(1),
        ]))
    );
}

#[test]
fn delete_many_to_many_join_cleaned_for_user() {
    let schema = many_to_many_schema();
    let backend = many_to_many_backend(&schema);

    let result = run(
        r#"
            users.first().unwrap().delete();
            roles.map($.users.len())
            "#,
        &schema,
        &backend,
    );

    assert_eq!(result, Some(Val::list(vec![int(1), int(0)])));
}

#[test]
fn delete_many_to_many_join_cleaned_for_role() {
    let schema = many_to_many_schema();
    let backend = many_to_many_backend(&schema);

    let result = run(
        r#"
            roles.first().unwrap().delete();
            users.map($.roles.len())
            "#,
        &schema,
        &backend,
    );

    assert_eq!(result, Some(Val::list(vec![int(1), int(0)])));
}

#[test]
fn update_record_fields_with_prev() {
    let schema = test_schema();
    let backend = test_backend(&schema);
    let kind = schema.enum_by_name("Kind").unwrap().0;

    let result = run(
        r#"
            items.insert({
                id = 1,
                name = "a",
                active = true,
                note = some("old"),
                pos = (1.0f, 2.0f),
                kind = Kind::A,
                maybe_kind = some(Kind::B),
            });
            items.first().unwrap().update({
                name = prev.name.concat("!"),
                note = none,
                pos = (3.0f, 4.0f),
                kind = Kind::B,
                maybe_kind = none,
            });
            items.first().unwrap() { id, name, active, note, pos, kind, maybe_kind }
            "#,
        &schema,
        &backend,
    );

    assert_eq!(
        result,
        Some(rec(vec![
            ("id", int(1)),
            ("name", str("a!")),
            ("active", bool(true)),
            ("note", Val::None),
            ("pos", Val::Tuple(vec![float(3.0), float(4.0)]),),
            ("kind", Val::Enum(kind, 1)),
            ("maybe_kind", Val::None),
        ]))
    );
}

#[test]
fn update_argument_values_precomputed() {
    let schema = test_schema();
    let backend = test_backend(&schema);

    let result = run(
        r#"
            items.insert([
                { id = 1, name = "a", active = true, pos = (0.0f, 0.0f), kind = Kind::A },
                { id = 2, name = "b", active = true, pos = (0.0f, 0.0f), kind = Kind::A },
            ]);
            items.update({
                name = items.first().unwrap().name.concat("x"),
            });
            items { name }
            "#,
        &schema,
        &backend,
    );

    assert_eq!(
        result,
        Some(Val::list(vec![
            rec(vec![("name", str("ax"))]),
            rec(vec![("name", str("ax"))]),
        ]))
    );
}

#[test]
fn update_optional_none_target_noop() {
    let schema = test_schema();
    let backend = test_backend(&schema);

    let result = run(
        r#"
            items.first()?.update({ name = "x" });
            items.len()
            "#,
        &schema,
        &backend,
    );

    assert_eq!(result, Some(int(0)));
}

#[test]
fn update_replace_array_fields() {
    let schema = array_schema();
    let backend = array_backend(&schema);

    let result = run(
        r#"
            rows.insert({
                id = 1,
                tags = ["a", "b"],
                nums = [1, 2],
                matrix = [[1], [2, 3]],
                extra = (7, ["x"]),
            });
            rows.first().unwrap().update({
                tags = ["c"],
                nums = [9],
                matrix = [[5, 6]],
                extra = (8, ["z"]),
            });
            rows.first().unwrap() { tags, nums, matrix, extra }
            "#,
        &schema,
        &backend,
    );

    assert_eq!(
        result,
        Some(rec(vec![
            ("tags", Val::list(vec![str("c")])),
            ("nums", Val::list(vec![int(9)])),
            ("matrix", Val::list(vec![Val::list(vec![int(5), int(6)])]),),
            ("extra", Val::Tuple(vec![int(8), Val::list(vec![str("z")])]),),
        ]))
    );
}

#[test]
fn update_replace_owning_refs() {
    let schema = ref_schema();
    let backend = ref_backend(&schema);

    let result = run(
        r#"
            users.insert({
                id = 1,
                name = "A",
                orders = [
                    { id = 10, total = 1 },
                    { id = 11, total = 2 },
                ],
                profile = some({ bio = "old" }),
            });
            users.first().unwrap().update({
                orders = [{ id = 20, total = 3 }],
                profile = some({ bio = "new" }),
            });
            users.first().unwrap() {
                orders = orders { id, total },
                bio = profile?.bio,
            }
            "#,
        &schema,
        &backend,
    );

    assert_eq!(
        result,
        Some(rec(vec![
            (
                "orders",
                Val::list(vec![rec(vec![("id", int(20)), ("total", int(3)),])])
            ),
            ("bio", some(str("new")))
        ]))
    );
}

#[test]
fn update_clear_optional_owning_single() {
    let schema = ref_schema();
    let backend = ref_backend(&schema);

    let result = run(
        r#"
            users.insert({
                id = 1,
                name = "A",
                orders = [],
                profile = some({ bio = "old" }),
            });
            users.first().unwrap().update({ profile = none });
            users.first().unwrap().profile
            "#,
        &schema,
        &backend,
    );

    assert_eq!(result, Some(Val::None));
}

#[test]
fn update_replace_nested_owning_children() {
    let schema = cascade_schema();
    let backend = cascade_backend(&schema);

    let result = run(
        r#"
            users.insert([{
                id = 1,
                name = "A",
                orders = [{
                    id = 10,
                    total = 1,
                    items = [{ id = 100 }],
                }],
            },
            {
                id = 2,
                name = "B",
                orders = [{
                    id = 30,
                    total = 3,
                    items = [{ id = 300 }],
                }],
            }]);
            users.first().unwrap().orders.first().unwrap().update({
                items = [{ id = 200 }, { id = 201 }],
            });
            users.find(name=="B").unwrap().update({
                orders = [{
                    id = 40,
                    total = 4,
                    items = [],
                },
                {
                    id = 30,
                    total = 5,
                    items = [{ id = 301 }, { id = 302 }],
                }],
            });
            [users.find(id==1), users.find(id==2)].transpose().unwrap() { orders = orders { id, total, items = items { id } } }
            "#,
        &schema,
        &backend,
    );

    assert_eq!(
        result,
        Some(Val::list(vec![
            rec(vec![(
                "orders",
                Val::list(vec![rec(vec![
                    ("id", int(10)),
                    ("total", int(1)),
                    (
                        "items",
                        Val::list(vec![
                            rec(vec![("id", int(200))]),
                            rec(vec![("id", int(201))])
                        ])
                    )
                ])])
            )]),
            rec(vec![(
                "orders",
                Val::list(vec![
                    rec(vec![
                        ("id", int(40)),
                        ("total", int(4)),
                        ("items", Val::list(vec![]))
                    ]),
                    rec(vec![
                        ("id", int(30)),
                        ("total", int(5)),
                        (
                            "items",
                            Val::list(vec![
                                rec(vec![("id", int(301))]),
                                rec(vec![("id", int(302))])
                            ])
                        )
                    ])
                ])
            )])
        ]))
    );
}

#[test]
fn update_empty_and_duplicate_targets() {
    let schema = test_schema();
    let backend = test_backend(&schema);

    let result = run(
        r#"
            items.update({ name = "x" });
            items.insert({ id = 1, name = "a", active = true, pos = (0.0f, 0.0f), kind = Kind::A });
            items.extend(items).update({ name = prev.name.concat("!") });
            items { name }
            "#,
        &schema,
        &backend,
    );

    assert_eq!(
        result,
        Some(Val::list(vec![rec(vec![("name", str("a!"))])]))
    );
}

#[test]
fn debug_rollback_returns_result_without_committing() {
    let schema = test_schema();
    let backend = test_backend(&schema);
    let (item, fields) = fields(&schema);

    let result = run_result_with_pushdown(
        r#"
            items.insert({
                id = 1,
                name = "a",
                active = true,
                pos = (0.0f, 0.0f),
                kind = Kind::A,
            });
            items.len()
            "#,
        &schema,
        &backend,
        true,
        true,
    )
    .unwrap();
    assert_eq!(result, Some(int(1)));

    let rows = backend
        .fetch(item, &fields, &FetchConstraints::EMPTY)
        .unwrap();
    assert!(rows.is_empty());
}
