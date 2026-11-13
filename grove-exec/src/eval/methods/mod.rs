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
use crate::value::{Scalar, Val, rewrap_optional, unwrap_optional_depth};

pub(super) fn eval_method(
    base: &TypedExpr,
    name: &str,
    args: &[TypedMethodArg],
    opt_depth: usize,
    node: NodeId,
    ctx: &mut EvalCtx<'_>,
    span: Span,
) -> Result<Val, ExecError> {
    let base = eval(base, ctx)?;
    let base = resolve_base_link(ctx, base)?;
    let (base, peeled) = unwrap_optional_depth(base, opt_depth)?;
    if peeled < opt_depth {
        return Ok(rewrap_optional(peeled, Val::None));
    }
    if let Some(cap) = ctx.plan.cap_plan(node)
        && ctx.applied_triggers.contains(&cap.trigger)
        && let (CapRewrite::Skip, "take" | "skip") = (cap.rewrite, name)
    {
        return Ok(rewrap_optional(opt_depth, base));
    }
    if let Some(cap) = ctx.plan.cap_plan(node)
        && ctx.applied_triggers.contains(&cap.trigger)
        && let (CapRewrite::First, "nth") = (cap.rewrite, name)
    {
        return Ok(rewrap_optional(
            opt_depth,
            eval_slice("first", base, &[], span)?,
        ));
    }

    let result = if let Some(sort_kind) = SortKind::from_str(name) {
        eval_sort(sort_kind, base, args, ctx, span)
    } else if name == "filter" {
        eval_filter(base, args, ctx)
    } else if name == "map" {
        eval_map(base, args, ctx)
    } else if let Some(kind) = ReduceKind::from_str(name) {
        eval_reduce(kind, base, ctx, span)
    } else if let Some(kind) = KeyedReduceKind::from_str(name) {
        eval_reduce_over(kind, base, args, ctx, span)
    } else if let Some(opt_kind) = OptMethodKind::from_str(name) {
        eval_optional(opt_kind, base, args, ctx, span)
    } else {
        let mut arg_vals = args
            .iter()
            .map(|arg| eval(&arg.expr, ctx))
            .collect::<Result<Vec<_>, _>>()?;
        if name == "flatten" {
            eval_flatten(base)
        } else if name == "extend" {
            eval_extend(base, arg_vals)
        } else if matches!(name, "take" | "skip" | "first" | "nth") {
            eval_slice(name, base, &arg_vals, span)
        } else if name == "len" && matches!(base, Val::List(_)) {
            eval_list_len(base)
        } else if name == "contains" && matches!(&base, Val::List(_)) {
            let Some(needle) = arg_vals.pop() else {
                return Err(ExecError::Integrity {
                    message: "`contains()` expects one argument (internal error)".to_string(),
                });
            };
            eval_list_contains(ctx, base, needle)
        } else {
            eval_scalar_method(&base, name, &arg_vals, span).map(Val::Scalar)
        }
    };

    Ok(rewrap_optional(opt_depth, result?))
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
