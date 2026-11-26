use std::collections::HashSet;

use grove_query::TypedExpr;
use grove_schema::validated::{FieldId, StructId, ValidatedSchema};

use super::cascade::delete_owned_children;
use super::insert::insert_owned_field;
use super::records::{
    SplitMode, collect_update_target_records, record_from_value, split_record_fields,
};
use crate::error::ExecError;
use crate::eval::{EvalCtx, eval};
use crate::storage::{LinkMode, WriteTxn};
use crate::value::{RecordVal, RowOrigin, Val, unwrap_optional_depth};

pub(super) fn exec_update(
    base: &TypedExpr,
    arg: &TypedExpr,
    opt_depth: usize,
    ctx: &mut EvalCtx<'_>,
    write: &mut dyn WriteTxn,
) -> Result<(), ExecError> {
    let target = eval(base, ctx)?;
    let target = if opt_depth > 0 {
        let (target, peeled) = unwrap_optional_depth(target, opt_depth)?;
        if peeled < opt_depth {
            return Ok(());
        }
        target
    } else {
        target
    };
    let targets = collect_update_target_records(target)?;
    if targets.is_empty() {
        return Ok(());
    }

    let mut updates = Vec::with_capacity(targets.len());
    for (origin, record) in targets {
        ctx.push_binding_scope("prev", record);
        let value = eval(arg, ctx);
        ctx.env.pop_scope();
        updates.push((origin, record_from_value(value?)?));
    }

    for (origin, record) in updates {
        let parts = resolve_update_parts(ctx.schema, origin.struct_id, record)?;
        apply_update(ctx, write, origin, parts)?;
    }
    Ok(())
}

pub(super) struct UpdateParts {
    /// ids and new values of modified base and array fields
    pub(super) base: Vec<(FieldId, Val)>,
    /// ids of modified array fields (values in `base`)
    pub(super) arrays: Vec<FieldId>,
    /// ids and new values of modified owning ref fields
    pub(super) owned: Vec<(FieldId, Val)>,
    /// ids and target row references of modified non-owning fields
    pub(super) links: Vec<(FieldId, Vec<RowOrigin>)>,
}

/// Apply a prepared update after all argument values have been computed
fn apply_update(
    ctx: &mut EvalCtx<'_>,
    write: &mut dyn WriteTxn,
    origin: RowOrigin,
    parts: UpdateParts,
) -> Result<(), ExecError> {
    let mut seen = HashSet::new();
    for (field_id, _) in &parts.owned {
        delete_owned_children(ctx, write, *field_id, origin.key, &mut seen)?;
    }
    for field_id in &parts.arrays {
        write.delete_array_field(*field_id, &[origin.key])?;
    }
    if !parts.base.is_empty() {
        write.update_row(origin.struct_id, origin.key, &parts.base)?;
    }
    for (field_id, targets) in &parts.links {
        write.link_row_refs(*field_id, origin.key, targets, LinkMode::Replace)?;
    }
    for (field_id, value) in parts.owned {
        insert_owned_field(ctx.schema, field_id, value, origin.key, write)?;
    }
    Ok(())
}

pub(super) fn resolve_update_parts(
    schema: &ValidatedSchema,
    struct_id: StructId,
    record: RecordVal,
) -> Result<UpdateParts, ExecError> {
    let parts = split_record_fields(schema, struct_id, record, SplitMode::Update)?;
    Ok(UpdateParts {
        base: parts.base,
        arrays: parts.arrays,
        owned: parts.owned,
        links: parts.links,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval::harness::{bool, float, int, rec, some, str};
    use crate::eval::mutations::tests::*;
    use crate::value::Key;
    use std::rc::Rc;

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
}
