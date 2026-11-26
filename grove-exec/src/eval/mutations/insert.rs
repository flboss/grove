use grove_query::{CollectionOriginKind, TypedExpr};
use grove_schema::validated::{Field, FieldId, RelationId, StructId, ValidatedSchema};

use super::links::link_targets;
use super::records::{collect_insert_records, record_from_value, record_parts};
use crate::error::ExecError;
use crate::eval::{EvalCtx, eval};
use crate::storage::{LinkMode, WriteTxn};
use crate::value::{CollectionOrigin, Key, RecordVal, RowOrigin, Val, unwrap_optional_depth};

pub(super) fn exec_insert(
    base: &TypedExpr,
    arg: &TypedExpr,
    opt_depth: usize,
    ctx: &mut EvalCtx<'_>,
    write: &mut dyn WriteTxn,
) -> Result<(), ExecError> {
    let Some(target) = resolve_insert_target(base, opt_depth, ctx)? else {
        return Ok(());
    };
    match target {
        InsertTarget::Link {
            struct_id: _,
            field,
            owner,
        } => {
            let arg = eval(arg, ctx)?;
            let targets = link_targets(ctx.schema, field, arg)?;
            write.link_row_refs(field, owner.key, &targets, LinkMode::Append)
        }
        target => {
            let arg = eval(arg, ctx)?;
            let records = collect_insert_records(arg)?;
            insert_group(ctx.schema, target, records, write)
        }
    }
}

enum InsertTarget {
    Root {
        struct_id: StructId,
    },
    Ref {
        struct_id: StructId,
        relation: RelationId,
        parent: Key,
        parent_is_first: bool,
    },
    Link {
        struct_id: StructId,
        field: FieldId,
        owner: RowOrigin,
    },
}

impl InsertTarget {
    fn struct_id(&self) -> StructId {
        match self {
            InsertTarget::Root { struct_id }
            | InsertTarget::Ref { struct_id, .. }
            | InsertTarget::Link { struct_id, .. } => *struct_id,
        }
    }
}

/// Resolve an insert target to a root or parented collection
fn resolve_insert_target(
    base: &TypedExpr,
    opt_depth: usize,
    ctx: &mut EvalCtx<'_>,
) -> Result<Option<InsertTarget>, ExecError> {
    let Some(origin) = base.provenance.collection else {
        return Err(ExecError::Unsupported {
            message: "insert target has no collection origin".to_string(),
        });
    };
    match origin {
        CollectionOriginKind::Root(root_idx) => {
            let root = ctx
                .schema
                .roots
                .get(root_idx)
                .ok_or_else(|| ExecError::Internal {
                    message: format!("unknown root {root_idx}"),
                })?;
            if opt_depth > 0 {
                let target = eval(base, ctx)?;
                let (_, peeled) = unwrap_optional_depth(target, opt_depth)?;
                if peeled < opt_depth {
                    return Ok(None);
                }
            }
            Ok(Some(InsertTarget::Root {
                struct_id: root.struct_id,
            }))
        }
        CollectionOriginKind::Ref(field_id) => {
            let target = eval(base, ctx)?;
            let target = if opt_depth > 0 {
                let (target, peeled) = unwrap_optional_depth(target, opt_depth)?;
                if peeled < opt_depth {
                    return Ok(None);
                }
                target
            } else {
                target
            };
            let Val::List(list) = target else {
                return Err(ExecError::Internal {
                    message: "nested insert target did not evaluate to a list".to_string(),
                });
            };
            let Some(CollectionOrigin::Ref { field, parent }) = list.origin else {
                return Err(ExecError::Internal {
                    message: "nested insert target has no ref collection origin".to_string(),
                });
            };
            if field != field_id {
                return Err(ExecError::Internal {
                    message: "nested insert origin field mismatch".to_string(),
                });
            }
            let Some(Field::Ref {
                target: struct_id,
                is_list: true,
                owning,
                ..
            }) = ctx.schema.field(field)
            else {
                return Err(ExecError::Internal {
                    message: "insert through unsupported reference".to_string(),
                });
            };
            if !*owning {
                let relation =
                    ctx.schema
                        .relation_of_field(field)
                        .ok_or_else(|| ExecError::Internal {
                            message: "insert target Ref has no relation".to_string(),
                        })?;
                if ctx
                    .schema
                    .relation_has_owning_endpoint(&ctx.schema.relations[relation.index()])
                {
                    return Err(ExecError::Internal {
                        message: "cannot insert through owning-relation backref".to_string(),
                    });
                }
                return Ok(Some(InsertTarget::Link {
                    struct_id: *struct_id,
                    field,
                    owner: parent,
                }));
            }
            let relation =
                ctx.schema
                    .relation_of_field(field)
                    .ok_or_else(|| ExecError::Internal {
                        message: "insert target Ref has no relation".to_string(),
                    })?;
            let parent_is_first = ctx.schema.relations[relation.index()].endpoints()[0] == field;
            Ok(Some(InsertTarget::Ref {
                struct_id: *struct_id,
                relation,
                parent: parent.key,
                parent_is_first,
            }))
        }
    }
}

/// Insert base records, then recursively insert owned child refs
fn insert_group(
    schema: &ValidatedSchema,
    target: InsertTarget,
    records: Vec<RecordVal>,
    write: &mut dyn WriteTxn,
) -> Result<(), ExecError> {
    if records.is_empty() {
        return Ok(());
    }
    let struct_id = target.struct_id();
    let mut values = Vec::with_capacity(records.len());
    let mut owned = Vec::with_capacity(records.len());
    let mut links = Vec::with_capacity(records.len());
    for record in records {
        let parts = record_parts(schema, struct_id, record)?;
        values.push(parts.base);
        owned.push(parts.owned);
        links.push(parts.links);
    }

    let keys = match &target {
        InsertTarget::Root { .. } => write.insert_rows(struct_id, &values)?,
        InsertTarget::Ref {
            relation,
            parent,
            parent_is_first,
            ..
        } => write.insert_rows_with_parent(
            struct_id,
            &values,
            *relation,
            *parent,
            *parent_is_first,
        )?,
        InsertTarget::Link { .. } => {
            return Err(ExecError::Internal {
                message: "link target reached record insertion".to_string(),
            });
        }
    };
    if keys.len() != values.len() {
        return Err(ExecError::Internal {
            message: "insert returned an unexpected number of row keys".to_string(),
        });
    }

    for ((children, links), key) in owned.into_iter().zip(links).zip(keys) {
        for (field_id, value) in children {
            insert_owned_field(schema, field_id, value, key, write)?;
        }
        for (field_id, targets) in links {
            write.link_row_refs(field_id, key, &targets, LinkMode::Append)?;
        }
    }
    Ok(())
}

pub(super) fn insert_owned_field(
    schema: &ValidatedSchema,
    field_id: FieldId,
    value: Val,
    parent: Key,
    write: &mut dyn WriteTxn,
) -> Result<(), ExecError> {
    let Some(Field::Ref {
        target,
        optional,
        is_list,
        owning,
        ..
    }) = schema.field(field_id)
    else {
        return Err(ExecError::Internal {
            message: "owned insert field is not a ref".to_string(),
        });
    };
    if !*owning {
        return Err(ExecError::Internal {
            message: "attempted recursive insert through non-owning ref".to_string(),
        });
    }
    let records = if *is_list {
        let Val::List(list) = value else {
            return Err(ExecError::Internal {
                message: "list ref field did not contain a list".to_string(),
            });
        };
        list.into_iter()
            .map(record_from_value)
            .collect::<Result<Vec<_>, _>>()?
    } else if *optional {
        match value {
            Val::None => Vec::new(),
            Val::Some(value) => vec![record_from_value(*value)?],
            other => {
                return Err(ExecError::Internal {
                    message: format!("optional ref field contained {other}"),
                });
            }
        }
    } else {
        vec![record_from_value(value)?]
    };
    if records.is_empty() {
        return Ok(());
    }
    let relation = schema
        .relation_of_field(field_id)
        .ok_or_else(|| ExecError::Internal {
            message: "owned insert field has no relation".to_string(),
        })?;
    let parent_is_first = schema.relations[relation.index()].endpoints()[0] == field_id;
    insert_group(
        schema,
        InsertTarget::Ref {
            struct_id: *target,
            relation,
            parent,
            parent_is_first,
        },
        records,
        write,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval::harness::{bool, float, int, rec, some, str};
    use crate::eval::mutations::tests::*;
    use crate::eval::{ExecOptions, exec_file_with_options};
    use crate::sqlite::SqliteBackend;
    use crate::storage::{FetchConstraints, Storage};
    use chrono::Utc;
    use rusqlite::Connection;

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
        assert_eq!(rows[0].values[0], int(1));
        assert_eq!(rows[1].values[0], int(2));
        assert_eq!(rows[0].values[6], Val::None);
        assert_eq!(rows[1].values[1], str("beta"));
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

        assert_eq!(result, Some(int(1)));
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
        let err =
            exec_file_with_options(&file, &backend, &schema, Utc::now(), ExecOptions::default())
                .unwrap_err();
        let ExecError::Integrity { message } = err else {
            panic!("expected integrity error, got {err:?}");
        };
        assert!(message.contains("no join key"), "{message}");
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
}
