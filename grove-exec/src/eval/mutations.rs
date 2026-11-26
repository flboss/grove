mod cascade;
mod delete;
mod insert;
mod links;
mod records;
mod update;

use delete::exec_delete;
use insert::exec_insert;
use update::exec_update;

use crate::error::ExecError;
use crate::eval::EvalCtx;
use crate::storage::WriteTxn;

use grove_query::TypedMutationStmt;

/// Dispatch typed mutation statement to its executor
pub(super) fn exec_mutation(
    stmt: &TypedMutationStmt,
    ctx: &mut EvalCtx<'_>,
    write: &mut dyn WriteTxn,
) -> Result<(), ExecError> {
    match stmt {
        TypedMutationStmt::Insert {
            base,
            arg,
            opt_depth,
            ..
        } => exec_insert(base, arg, *opt_depth, ctx, write),
        TypedMutationStmt::Delete {
            base, opt_depth, ..
        } => exec_delete(base, *opt_depth, ctx, write),
        TypedMutationStmt::Update {
            base,
            arg,
            opt_depth,
            ..
        } => exec_update(base, arg, *opt_depth, ctx, write),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval::harness::{assert_pushdown_parity, int};
    use crate::eval::{ExecOptions, exec_file_with_options};
    use crate::sqlite::SqliteBackend;
    use crate::storage::{FetchConstraints, Storage};
    use crate::value::Val;

    use chrono::Utc;
    use grove_query::TypedQueryFile;
    use grove_schema::validated::{FieldId, StructId, ValidatedSchema};
    use rusqlite::Connection;

    pub(super) fn test_schema() -> ValidatedSchema {
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

    pub(super) fn test_backend(schema: &ValidatedSchema) -> SqliteBackend<'_> {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE items(id INTEGER, name TEXT, active INTEGER, note TEXT,
             x REAL, y REAL, kind INTEGER, maybe_kind INTEGER)",
        )
        .unwrap();
        SqliteBackend::new(conn, schema).unwrap()
    }

    pub(super) fn typed(source: &str, schema: &ValidatedSchema) -> TypedQueryFile {
        let (file, parse_diags) = grove_query::parse_query(source);
        assert!(parse_diags.is_empty(), "{parse_diags:?}");
        let (typed, diags) = grove_query::typecheck::typecheck(file.unwrap(), schema);
        assert!(diags.is_empty(), "{diags:?}");
        typed.unwrap()
    }

    pub(super) fn run(
        source: &str,
        schema: &ValidatedSchema,
        backend: &SqliteBackend<'_>,
    ) -> Option<Val> {
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

    pub(super) fn fields(schema: &ValidatedSchema) -> (StructId, Vec<FieldId>) {
        let item = schema.roots[0].struct_id;
        let fields = ["id", "name", "active", "note", "pos", "kind", "maybe_kind"]
            .iter()
            .map(|name| schema.struct_field(item, name).unwrap().0)
            .collect();
        (item, fields)
    }

    pub(super) fn ref_schema() -> ValidatedSchema {
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

    pub(super) fn ref_backend(schema: &ValidatedSchema) -> SqliteBackend<'_> {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE users(id INTEGER, name TEXT);
             CREATE TABLE orders(id INTEGER, total INTEGER, user_id INTEGER);
             CREATE TABLE profiles(bio TEXT, user_id INTEGER);",
        )
        .unwrap();
        SqliteBackend::new(conn, schema).unwrap()
    }

    pub(super) fn array_schema() -> ValidatedSchema {
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

    pub(super) fn array_backend(schema: &ValidatedSchema) -> SqliteBackend<'_> {
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

    pub(super) fn cascade_schema() -> ValidatedSchema {
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

    pub(super) fn cascade_backend(schema: &ValidatedSchema) -> SqliteBackend<'_> {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE users(id INTEGER, name TEXT);
             CREATE TABLE orders(id INTEGER, total INTEGER, user_id INTEGER);
             CREATE TABLE items(id INTEGER, order_id INTEGER);",
        )
        .unwrap();
        SqliteBackend::new(conn, schema).unwrap()
    }

    pub(super) fn one_to_many_schema() -> ValidatedSchema {
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

    pub(super) fn one_to_many_backend(schema: &ValidatedSchema) -> SqliteBackend<'_> {
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

    pub(super) fn self_ref_schema() -> ValidatedSchema {
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

    pub(super) fn self_ref_backend(schema: &ValidatedSchema) -> SqliteBackend<'_> {
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

    pub(super) fn many_to_many_schema() -> ValidatedSchema {
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

    pub(super) fn many_to_many_backend(schema: &ValidatedSchema) -> SqliteBackend<'_> {
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

    pub(super) fn nested_link_schema() -> ValidatedSchema {
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

    pub(super) fn nested_link_backend(schema: &ValidatedSchema) -> SqliteBackend<'_> {
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

    pub(super) fn mutation_parts_schema() -> ValidatedSchema {
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

    pub(super) fn mutation_parts_ids(schema: &ValidatedSchema) -> (StructId, StructId, StructId) {
        (
            schema.struct_by_name("User").unwrap().0,
            schema.struct_by_name("Order").unwrap().0,
            schema.struct_by_name("Tag").unwrap().0,
        )
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
}
