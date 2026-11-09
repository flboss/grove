mod filter;
mod list;
mod map;
mod optional;
mod reduce;
mod scalar;
mod sort;

use std::collections::HashMap;
use std::rc::Rc;

use grove_query::typecheck::types::{TypedExpr, TypedMethodArg};
use grove_types::{NodeId, Span};

use self::filter::eval_filter;
use self::list::{eval_extend, eval_flatten, eval_list_contains, eval_list_len, eval_slice};
use self::map::eval_map;
use self::optional::{OptMethodKind, eval_optional};
use self::reduce::{KeyedReduceKind, ReduceKind, eval_reduce, eval_reduce_over};
use self::scalar::eval_scalar_method;
use self::sort::{SortKind, eval_sort};
use super::data::resolve_base_link;
use super::{EvalCtx, eval};
use crate::error::ExecError;
use crate::pushdown::CapRewrite;
use crate::value::{Scalar, Val};

pub(super) fn eval_method(
    base: &TypedExpr,
    name: &str,
    args: &[TypedMethodArg],
    optional: bool,
    node: NodeId,
    ctx: &mut EvalCtx<'_>,
    span: Span,
) -> Result<Val, ExecError> {
    let base = eval(base, ctx)?;
    let base = resolve_base_link(ctx, base)?;
    // `?.` on none returns none without evaluating args
    let base = match (optional, base) {
        (true, Val::None) => return Ok(Val::None),
        (true, Val::Some(v)) => *v,
        (_, v) => v,
    };
    if let Some(rewrite) = ctx.plan.cap_rewrite(node) {
        match (rewrite, name) {
            (CapRewrite::Skip, "take" | "skip") => return Ok(base),
            (CapRewrite::First, "nth") => return eval_slice("first", base, optional, &[], span),
            _ => {}
        }
    }
    if let Some(sort_kind) = SortKind::from_str(name) {
        return eval_sort(sort_kind, base, optional, args, ctx, span);
    }
    if name == "filter" {
        return eval_filter(base, optional, args, ctx);
    }
    if name == "map" {
        return eval_map(base, optional, args, ctx);
    }
    if let Some(kind) = ReduceKind::from_str(name) {
        return eval_reduce(kind, base, optional, ctx, span);
    }
    if let Some(kind) = KeyedReduceKind::from_str(name) {
        return eval_reduce_over(kind, base, optional, args, ctx, span);
    }
    if let Some(opt_kind) = OptMethodKind::from_str(name) {
        return eval_optional(opt_kind, base, optional, args, ctx, span);
    }
    let arg_vals = args
        .iter()
        .map(|arg| eval(&arg.expr, ctx))
        .collect::<Result<Vec<_>, _>>()?;
    if name == "flatten" {
        return eval_flatten(base, optional);
    }
    if name == "extend" {
        return eval_extend(base, optional, arg_vals);
    }
    if matches!(name, "take" | "skip" | "first" | "nth") {
        return eval_slice(name, base, optional, &arg_vals, span);
    }
    if name == "len" && matches!(base, Val::List(_)) {
        return eval_list_len(base, optional);
    }
    if name == "contains" && matches!(&base, Val::List(_)) {
        let [needle] = arg_vals.as_slice() else {
            return Err(ExecError::Integrity {
                message: "`contains()` expects one argument (internal error)".to_string(),
            });
        };
        return eval_list_contains(base, optional, needle);
    }
    let scalar = eval_scalar_method(&base, name, &arg_vals, span)?;
    let value = Val::Scalar(scalar);
    Ok(if optional {
        Val::Some(Box::new(value))
    } else {
        value
    })
}

fn method_arg<'a>(args: &'a [Val], method: &str, index: usize) -> Result<&'a Val, ExecError> {
    args.get(index).ok_or_else(|| ExecError::Integrity {
        message: format!("missing argument {index} for `{method}` (internal error)"),
    })
}

pub(super) fn method_string_arg<'a>(args: &'a [Val], method: &str) -> Result<&'a str, ExecError> {
    let Val::Scalar(Scalar::String(s)) = method_arg(args, method, 0)? else {
        return Err(ExecError::Integrity {
            message: format!("`{method}` expects a String argument (internal error)"),
        });
    };
    Ok(s.as_str())
}

pub(super) fn method_int_arg(args: &[Val], method: &str) -> Result<i64, ExecError> {
    let Val::Scalar(Scalar::Int(n)) = method_arg(args, method, 0)? else {
        return Err(ExecError::Integrity {
            message: format!("`{method}` expects an Int argument (internal error)"),
        });
    };
    Ok(*n)
}

pub(super) fn push_element_scope(ctx: &mut EvalCtx<'_>, item: &Val) -> bool {
    let (scope, map_element): (HashMap<Rc<str>, Val>, _) = match item {
        Val::Record(record) => (
            record
                .iter()
                .map(|(name, value)| (ctx.interner.intern(name.as_ref()), value.clone()))
                .collect(),
            false,
        ),
        _ => {
            ctx.env.element_depth += 1;
            (
                [(
                    ctx.interner.intern("$".repeat(ctx.env.element_depth)),
                    item.clone(),
                )]
                .into(),
                true,
            )
        }
    };
    ctx.env.push_scope(scope);
    map_element
}

pub(super) fn pop_element_scope(ctx: &mut EvalCtx<'_>, map_element: bool) {
    ctx.env.pop_scope();
    if map_element {
        ctx.env.element_depth -= 1;
    }
}

pub(super) fn push_map_scope(ctx: &mut EvalCtx<'_>, item: Val) {
    ctx.env.element_depth += 1;
    let scope: HashMap<Rc<str>, Val> =
        [(ctx.interner.intern("$".repeat(ctx.env.element_depth)), item)].into();
    ctx.env.push_scope(scope);
}

pub(super) fn pop_map_scope(ctx: &mut EvalCtx<'_>) {
    ctx.env.pop_scope();
    ctx.env.element_depth -= 1;
}
