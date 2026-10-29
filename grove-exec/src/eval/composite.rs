use std::sync::Arc;

use grove_query::typecheck::types::{TypedExpr, TypedProjectionItem};
use grove_types::Spanned;

use super::{EvalCtx, eval};
use crate::error::ExecError;
use crate::eval::data::resolve_projection_refs;
use crate::value::Val;

pub(crate) fn eval_tuple(elements: &[TypedExpr], ctx: &mut EvalCtx<'_>) -> Result<Val, ExecError> {
    let mut items = Vec::with_capacity(elements.len());
    for element in elements {
        items.push(eval(element, ctx)?);
    }
    Ok(Val::Tuple(items))
}

pub(crate) fn eval_array(elements: &[TypedExpr], ctx: &mut EvalCtx<'_>) -> Result<Val, ExecError> {
    let mut items = Vec::with_capacity(elements.len());
    for element in elements {
        items.push(eval(element, ctx)?);
    }
    Ok(Val::List(items))
}

pub(crate) fn eval_struct(
    fields: &[(Spanned<String>, TypedExpr)],
    ctx: &mut EvalCtx<'_>,
) -> Result<Val, ExecError> {
    let mut record = Vec::with_capacity(fields.len());
    for (name, value) in fields {
        let value = eval(value, ctx)?;
        record.push((ctx.interner.intern(&name.value), value));
    }
    resolve_projection_refs(ctx, Val::Record(record))
}

pub(crate) fn eval_projection(
    base: &TypedExpr,
    items: &[TypedProjectionItem],
    ctx: &mut EvalCtx<'_>,
) -> Result<Val, ExecError> {
    let base = eval(base, ctx)?;
    match base {
        Val::Record(record) => {
            let projected = project_record(record, items, ctx)?;
            resolve_projection_refs(ctx, projected)
        }
        Val::List(rows) => {
            let mut projected = Vec::with_capacity(rows.len());
            for row in rows {
                match row {
                    Val::Record(record) => projected.push(project_record(record, items, ctx)?),
                    Val::None => projected.push(Val::None),
                    other => {
                        return Err(ExecError::Integrity {
                            message: format!(
                                "projection over non-record list element: {other} (internal error)"
                            ),
                        });
                    }
                }
            }
            resolve_projection_refs(ctx, Val::List(projected))
        }
        other => Err(ExecError::Integrity {
            message: format!("projection over non-record value: {other} (internal error)"),
        }),
    }
}

fn project_record(
    record: impl IntoIterator<Item = (Arc<str>, Val)>,
    items: &[TypedProjectionItem],
    ctx: &mut EvalCtx<'_>,
) -> Result<Val, ExecError> {
    let scope = record
        .into_iter()
        .map(|(name, value)| (ctx.interner.intern(name.as_ref()), value))
        .collect();
    ctx.env.push_scope(scope);
    let mut projected = Vec::with_capacity(items.len());
    for item in items {
        let value = eval(&item.value, ctx)?;
        projected.push((ctx.interner.intern(&item.alias.value), value));
    }
    ctx.env.pop_scope();
    Ok(Val::Record(projected))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval::harness::*;

    #[test]
    fn projection_ref_items_resolved() {
        match run_seeded("users { m = manager }") {
            Ok(Val::List(rows)) => {
                assert_eq!(rows.len(), 5);
                let Val::Record(first) = &rows[0] else {
                    panic!("expected record");
                };
                match first.iter().find(|(name, _)| name.as_ref() == "m") {
                    Some((_, Val::Some(manager))) => {
                        let Val::Record(manager) = manager.as_ref() else {
                            panic!("expected manager record");
                        };
                        assert!(manager.iter().any(|(name, value)| {
                            name.as_ref() == "name" && *value == str("Bob")
                        }));
                        assert!(manager.iter().any(|(name, value)| {
                            name.as_ref() == "manager" && matches!(value, Val::Ref { .. })
                        }));
                    }
                    got => panic!("expected some(manager), got {got:?}"),
                }
                let Val::Record(second) = &rows[1] else {
                    panic!("expected record");
                };
                assert!(
                    second.iter().any(|(name, value)| {
                        name.as_ref() == "m" && matches!(value, Val::None)
                    })
                );
            }
            got => panic!("expected projected users, got {got:?}"),
        }
    }

    #[test]
    fn projection_refs_batched_shared_mixed() {
        match run_seeded("users { m = manager, ms = manager, o = orders }") {
            Ok(Val::List(rows)) => {
                assert_eq!(rows.len(), 5);
                let Val::Record(first) = &rows[0] else {
                    panic!("expected record");
                };
                assert_eq!(first.len(), 3);
                let manager = |field: &str| match first.iter().find(|(n, _)| n.as_ref() == field) {
                    Some((_, Val::Some(m))) => match m.as_ref() {
                        Val::Record(m) => m
                            .iter()
                            .find(|(n, _)| n.as_ref() == "name")
                            .map(|(_, v)| v.clone()),
                        other => panic!("expected manager record, got {other}"),
                    },
                    got => panic!("expected some(manager) for {field}, got {got:?}"),
                };
                assert_eq!(manager("m"), manager("ms"));
                assert_eq!(manager("m"), Some(str("Bob")));
                match first.iter().find(|(name, _)| name.as_ref() == "o") {
                    Some((_, Val::List(orders))) => assert_eq!(orders.len(), 2),
                    got => panic!("expected orders list, got {got:?}"),
                }
            }
            got => panic!("expected projected users, got {got:?}"),
        }
    }

    #[test]
    fn projection_chained_refs_resolved() {
        match run_seeded("users { m = manager } { m }") {
            Ok(Val::List(rows)) => {
                assert_eq!(rows.len(), 5);
                match &rows[0] {
                    Val::Record(outer) => match outer.as_slice() {
                        [(name, Val::Some(manager))] => {
                            assert_eq!(name.as_ref(), "m");
                            let Val::Record(manager) = manager.as_ref() else {
                                panic!("expected manager record");
                            };
                            assert!(manager.iter().any(|(name, value)| {
                                name.as_ref() == "name" && *value == str("Bob")
                            }));
                            assert!(manager.iter().any(|(name, value)| {
                                name.as_ref() == "manager" && matches!(value, Val::Ref { .. })
                            }));
                        }
                        other => panic!("expected single m item, got {other:?}"),
                    },
                    other => panic!("expected record, got {other}"),
                }
            }
            got => panic!("expected projected users, got {got:?}"),
        }
    }

    #[test]
    fn projection_untouched_nested_refs_unresolved() {
        match run_seeded("users { orders }") {
            Ok(Val::List(rows)) => {
                assert_eq!(rows.len(), 5);
                let Val::Record(first) = &rows[0] else {
                    panic!("expected record");
                };
                match first.iter().find(|(name, _)| name.as_ref() == "orders") {
                    Some((_, Val::List(orders))) => {
                        assert_eq!(orders.len(), 2);
                        for order in orders {
                            let Val::Record(order) = order else {
                                panic!("expected order record, got {order}");
                            };
                            assert!(order.iter().any(|(name, value)| {
                                name.as_ref() == "user" && matches!(value, Val::Ref { .. })
                            }));
                        }
                    }
                    got => panic!("expected orders list, got {got:?}"),
                }
                let Val::Record(fifth) = &rows[4] else {
                    panic!("expected record");
                };
                assert!(fifth.iter().any(|(name, value)| {
                    name.as_ref() == "orders"
                        && matches!(value, Val::List(items) if items.is_empty())
                }));
            }
            got => panic!("expected projected users, got {got:?}"),
        }
    }

    #[test]
    fn projection_nested_levels_refs_resolved() {
        match run_seeded("users { m = manager, o = orders { amount } }") {
            Ok(Val::List(rows)) => {
                assert_eq!(rows.len(), 5);
                let Val::Record(first) = &rows[0] else {
                    panic!("expected record");
                };
                assert!(first.iter().any(|(name, value)| {
                    name.as_ref() == "m" && matches!(value, Val::Some(_))
                }));
                match first.iter().find(|(name, _)| name.as_ref() == "o") {
                    Some((_, Val::List(orders))) => {
                        assert_eq!(orders.len(), 2);
                        for order in orders {
                            let Val::Record(order) = order else {
                                panic!("expected order record, got {order}");
                            };
                            assert_eq!(order.len(), 1);
                        }
                    }
                    got => panic!("expected orders list, got {got:?}"),
                }
            }
            got => panic!("expected projected users, got {got:?}"),
        }
    }

    #[test]
    fn projection_struct_literal_refs_resolved() {
        match run_seeded("users { s = { m = manager, n = name } }") {
            Ok(Val::List(rows)) => {
                assert_eq!(rows.len(), 5);
                let Val::Record(first) = &rows[0] else {
                    panic!("expected record");
                };
                match first.iter().find(|(name, _)| name.as_ref() == "s") {
                    Some((_, Val::Record(nested))) => {
                        assert_eq!(nested.len(), 2);
                        assert!(
                            nested.iter().any(|(name, value)| {
                                name.as_ref() == "n" && *value == str("Ada")
                            })
                        );
                        assert!(nested.iter().any(|(name, value)| {
                            name.as_ref() == "m" && matches!(value, Val::Some(_))
                        }));
                    }
                    got => panic!("expected nested record, got {got:?}"),
                }
            }
            got => panic!("expected projected users, got {got:?}"),
        }
    }

    #[test]
    fn projection_flat_fields() {
        assert_eq!(
            run_seeded("roles { name }"),
            Ok(Val::List(vec![
                rec(vec![("name", str("admin"))]),
                rec(vec![("name", str("editor"))]),
            ]))
        );
    }

    #[test]
    fn projection_alias() {
        match run_seeded("users { n = name, id }") {
            Ok(Val::List(rows)) => {
                assert_eq!(rows.len(), 5);
                for row in &rows {
                    let Val::Record(fields) = row else {
                        panic!("expected record, got {row}");
                    };
                    assert_eq!(fields.len(), 2);
                    assert_eq!(fields[0].0.as_ref(), "n");
                    assert_eq!(fields[1].0.as_ref(), "id");
                }
                let Val::Record(first) = &rows[0] else {
                    panic!("expected record");
                };
                assert_eq!(first[0].1, str("Ada"));
                assert_eq!(first[1].1, int(1));
            }
            got => panic!("expected user rows, got {got:?}"),
        }
    }

    #[test]
    fn projection_chained() {
        assert_eq!(
            run_seeded("users { name } { name }"),
            Ok(Val::List(vec![
                rec(vec![("name", str("Ada"))]),
                rec(vec![("name", str("Bob"))]),
                rec(vec![("name", str("Cyd"))]),
                rec(vec![("name", str("Dan"))]),
                rec(vec![("name", str("Eli"))]),
            ]))
        );
    }

    #[test]
    fn projection_struct_literal() {
        assert_eq!(
            run_src("{ a = 1, b = 2 } { b }"),
            Ok(rec(vec![("b", int(2))]))
        );
    }

    #[test]
    fn projection_let_field_access() {
        assert_eq!(
            run_seeded("let u = users; u.name"),
            Ok(Val::List(vec![
                str("Ada"),
                str("Bob"),
                str("Cyd"),
                str("Dan"),
                str("Eli"),
            ]))
        );
        assert_eq!(
            run_seeded("let r = roles { name }; r.name"),
            Ok(Val::List(vec![str("admin"), str("editor")]))
        );
    }

    #[test]
    fn eval_tuple_literal() {
        assert_eq!(
            run_src("(1, \"hi\")"),
            Ok(Val::Tuple(vec![int(1), str("hi")]))
        );
        assert_eq!(
            run_src("(1, (2, 3))"),
            Ok(Val::Tuple(vec![int(1), Val::Tuple(vec![int(2), int(3)])]))
        );
        assert_eq!(
            run_src("let x = 1; (x, x)"),
            Ok(Val::Tuple(vec![int(1), int(1)]))
        );
        assert_eq!(
            run_src("if true { (1, 2) } else { (3, 4) }"),
            Ok(Val::Tuple(vec![int(1), int(2)]))
        );
    }

    #[test]
    fn eval_array_literal() {
        assert_eq!(
            run_src("[1, 2, 3]"),
            Ok(Val::List(vec![int(1), int(2), int(3)]))
        );
        assert_eq!(
            run_src("[[1], [2]]"),
            Ok(Val::List(vec![
                Val::List(vec![int(1)]),
                Val::List(vec![int(2)])
            ]))
        );
        assert_eq!(run_src("[1 + 1, 3]"), Ok(Val::List(vec![int(2), int(3)])));
        assert_eq!(
            run_src("let x = 1; [x, x]"),
            Ok(Val::List(vec![int(1), int(1)]))
        );
    }

    #[test]
    fn eval_struct_literal() {
        assert_eq!(
            run_src("{name = \"Alice\", age = 30}"),
            Ok(rec(vec![("name", str("Alice")), ("age", int(30))]))
        );
        assert_eq!(run_src("let x = 1; {a = x}"), Ok(rec(vec![("a", int(1))])));
        assert_eq!(
            run_src("{a = {b = 1}}"),
            Ok(rec(vec![("a", rec(vec![("b", int(1))]))]))
        );
        assert_eq!(
            run_src("{a = [1, 2]}"),
            Ok(rec(vec![("a", Val::List(vec![int(1), int(2)]))]))
        );
    }
}
