use std::sync::Arc;

use grove_query::typecheck::types::{TypedExpr, TypedProjectionItem};
use grove_types::Spanned;

use super::{EvalCtx, eval};
use crate::error::ExecError;
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
    Ok(Val::Record(record))
}

pub(crate) fn eval_projection(
    base: &TypedExpr,
    items: &[TypedProjectionItem],
    ctx: &mut EvalCtx<'_>,
) -> Result<Val, ExecError> {
    let base = eval(base, ctx)?;
    match base {
        Val::Record(record) => project_record(record, items, ctx),
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
            Ok(Val::List(projected))
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
        // TODO: avoid allocation
        .map(|(name, value)| (name.to_string(), value))
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
