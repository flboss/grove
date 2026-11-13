use grove_query::typecheck::types::TypedMethodArg;

use super::super::data::{owning_refs_in, prefetch_owning};
use super::super::{EvalCtx, eval};
use super::{pop_map_scope, push_map_scope};
use crate::error::ExecError;
use crate::value::Val;

pub(super) fn eval_map(
    base: Val,
    args: &[TypedMethodArg],
    ctx: &mut EvalCtx<'_>,
) -> Result<Val, ExecError> {
    let mut items = match base {
        Val::List(items) => items,
        other => {
            return Err(ExecError::Integrity {
                message: format!("`map()` called on non-list value: {other} (internal error)"),
            });
        }
    };
    let [arg] = args else {
        return Err(ExecError::Integrity {
            message: "`map()` expects one argument (internal error)".to_string(),
        });
    };
    let fields = owning_refs_in(&arg.expr, ctx.schema, ctx.plan.prefetch_skip());
    prefetch_owning(ctx, &mut items, &fields)?;
    let mut mapped = Vec::with_capacity(items.len());
    for item in items {
        push_map_scope(ctx, item);
        let value = eval(&arg.expr, ctx);
        pop_map_scope(ctx);
        mapped.push(value?);
    }
    Ok(Val::List(mapped))
}

#[cfg(test)]
mod tests {
    use crate::error::ExecError;
    use crate::eval::harness::*;
    use crate::value::Val;
    use rust_decimal::dec;

    #[test]
    fn method_map() {
        let run_map = |src: &str| match run_seeded(src) {
            Ok(Val::List(elements)) => elements,
            got => panic!("expected List, got: {got:?}"),
        };
        assert_eq!(
            run_map("[1, 2, 4].map($ - 2)"),
            vec![int(-1), int(0), int(2)]
        );
        assert_eq!(run_map("[1, 2, 3].map($)"), vec![int(1), int(2), int(3)]);
        assert_eq!(
            run_map("[1, 2, 3].map($ + 1).map($ * 2)"),
            vec![int(4), int(6), int(8)]
        );
        assert_eq!(
            run_map("[(1, \"a\"), (2, \"b\")].map($.1)"),
            vec![str("a"), str("b")]
        );
        assert_eq!(
            run_map("[1, 2].map(($, $))"),
            vec![
                Val::Tuple(vec![int(1), int(1)]),
                Val::Tuple(vec![int(2), int(2)])
            ]
        );
        assert_eq!(run_map("[1].skip(1).map($ / 0)"), Vec::new());
        match run_src("[1].map($ / 0)") {
            Err(ExecError::Abort { message, .. }) => {
                assert!(message.contains("division by zero"), "got: {message}")
            }
            got => panic!("expected abort, got {got:?}"),
        }
        assert_eq!(
            run_map("users.map($.name)"),
            [str("Ada"), str("Bob"), str("Cyd"), str("Dan"), str("Eli")]
        );
        assert_eq!(
            run_map("users.map($.balance * 2.0)"),
            [
                dec(dec!(60)),
                dec(dec!(0)),
                dec(dec!(-10.5)),
                dec(dec!(2000000)),
                dec(dec!(0.2))
            ]
        );
        assert_eq!(
            run_map("users.map($.active)"),
            [bool(true), bool(true), bool(false), bool(true), bool(false)]
        );
        assert_eq!(
            run_map("users.map($.nickname)"),
            [
                some(str("A")),
                Val::None,
                some(str("C")),
                Val::None,
                some(str("E"))
            ]
        );
        assert_eq!(
            run_map("users.map($.id.to_string())"),
            [str("1"), str("2"), str("3"), str("4"), str("5")]
        );
        assert_eq!(
            run_map("users.map($.pos.1)"),
            [float(-2.5), float(0.0), float(7.0), float(2.0), float(-0.5)]
        );
        assert_eq!(
            run_map("users.map({ n = $.name })"),
            [
                rec(vec![("n", str("Ada"))]),
                rec(vec![("n", str("Bob"))]),
                rec(vec![("n", str("Cyd"))]),
                rec(vec![("n", str("Dan"))]),
                rec(vec![("n", str("Eli"))])
            ]
        );
        assert_eq!(
            run_map("users.map(if $.active { 1 } else { 0 })"),
            [int(1), int(1), int(0), int(1), int(0)]
        );
        assert_eq!(
            run_seeded("users.skip(5).map($.id / 0)"),
            Ok(Val::List(vec![]))
        );
    }

    #[test]
    fn method_map_nesting() {
        let run_map = |src: &str| match run_seeded(src) {
            Ok(Val::List(elements)) => elements,
            got => panic!("expected List, got: {got:?}"),
        };
        assert_eq!(
            run_map("[[1, 2], [3]].map($.map($$ + 1))"),
            vec![Val::List(vec![int(2), int(3)]), Val::List(vec![int(4)])]
        );
        assert_eq!(
            run_map("[[1, 2], [3]].map($.map($$ + $.len()))"),
            vec![Val::List(vec![int(3), int(4)]), Val::List(vec![int(4)])]
        );
        assert_eq!(
            run_map("[[[1]], [[2, 3]]].map($.map($$.map($$$ + $$.len() + $.len())))"),
            vec![
                Val::List(vec![Val::List(vec![int(3)])]),
                Val::List(vec![Val::List(vec![int(5), int(6)])])
            ]
        );
        assert_eq!(
            run_map("[1, 2].map(users[id > $].name)"),
            vec![
                Val::List(vec![str("Bob"), str("Cyd"), str("Dan"), str("Eli")]),
                Val::List(vec![str("Cyd"), str("Dan"), str("Eli")]),
            ]
        );
        assert_eq!(
            run_map("users { o = orders.map($.amount + 5) }.o"),
            vec![
                Val::List(vec![int(105), int(205)]),
                Val::List(vec![int(405)]),
                Val::List(vec![]),
                Val::List(vec![]),
                Val::List(vec![]),
            ]
        );
    }

    #[test]
    fn method_map_optional() {
        assert_eq!(
            run_seeded("users.first()?.orders?.map($.amount/3)"),
            Ok(some(Val::List(vec![int(33), int(66)])))
        );
        assert_eq!(
            run_seeded("users.skip(5).first()?.orders?.map($.amount)"),
            Ok(Val::None)
        );
        assert_eq!(
            run_seeded(
                "some([some([1,11]), some([2,22,222]), none])?.map($?.map($$ * 2 - 3 * $?.len().unwrap_or(0)))"
            ),
            Ok(some(Val::List(vec![
                some(Val::List(vec![int(-4), int(16)])),
                some(Val::List(vec![int(-5), int(35), int(435)])),
                Val::None
            ])))
        );
    }
}
