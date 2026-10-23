use grove_query::typecheck::types::TypedExpr;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval::harness::*;

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
