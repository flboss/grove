mod filter;
mod list;
mod map;
mod optional;
mod reduce;
mod scalar;
mod sort;

use std::collections::HashMap;
use std::rc::Rc;

use grove_query::methods::{self, MethodId};
use grove_query::typecheck::types::{TypedExpr, TypedExprKind, TypedMethodArg};

use self::filter::eval_filter;
use self::list::{
    eval_list_compact, eval_list_contains, eval_list_extend, eval_list_flatten,
    eval_list_is_unique, eval_list_join, eval_list_len, eval_list_slice, eval_list_transpose,
    eval_list_unique, eval_list_unique_over,
};
use self::map::{eval_list_map, eval_optional_map};
use self::optional::{OptMethodKind, eval_optional, eval_optional_flatten};
use self::reduce::{KeyedReduceKind, ReduceKind, eval_reduce, eval_reduce_over};
use self::scalar::{eval_scalar_method, eval_string_split};
use self::sort::{SortKind, eval_sort};
use crate::error::ExecError;
use crate::eval::data::resolve_base_link;
use crate::eval::{EvalCtx, eval};
use crate::pushdown::CapRewrite;
use crate::value::{Scalar, Val, rewrap_optional, unwrap_optional_depth};

pub(super) fn eval_method(expr: &TypedExpr, ctx: &mut EvalCtx<'_>) -> Result<Val, ExecError> {
    let TypedExprKind::Method {
        method,
        base,
        name,
        args,
        opt_depth,
    } = &expr.kind
    else {
        unreachable!("eval_method called on a non-method expression");
    };
    let method = *method;
    let opt_depth = *opt_depth;
    let node = expr.id;
    let span = expr.span;
    let name = name.value.as_str();
    let base = eval(base, ctx)?;
    let base = resolve_base_link(ctx, base)?;
    let (base, peeled) = unwrap_optional_depth(base, opt_depth)?;
    if peeled < opt_depth {
        return Ok(rewrap_optional(peeled, Val::None));
    }
    if let Some(cap) = ctx.plan.cap_plan(node)
        && ctx.applied_triggers.contains(&cap.trigger)
    {
        match (cap.rewrite, method) {
            (CapRewrite::Skip, MethodId::ListTake | MethodId::ListSkip) => {
                return Ok(rewrap_optional(opt_depth, base));
            }
            (CapRewrite::First, MethodId::ListNth) => {
                return Ok(rewrap_optional(
                    opt_depth,
                    eval_list_slice(MethodId::ListFirst, base, &[], span)?,
                ));
            }
            _ => {}
        }
    }

    let result = match method {
        MethodId::ListSortAsc
        | MethodId::ListSortDesc
        | MethodId::ListSortAscOver
        | MethodId::ListSortDescOver => eval_sort(
            SortKind::from_method(method).expect("sort method id has a sort kind"),
            base,
            args,
            ctx,
            span,
        ),
        MethodId::ListFilter => eval_filter(base, args, ctx),
        MethodId::ListMap => eval_list_map(base, args, ctx),
        MethodId::ListUnique => eval_list_unique(base),
        MethodId::ListUniqueOver => eval_list_unique_over(base, args, ctx),
        MethodId::ListIsUnique => eval_list_is_unique(base),
        MethodId::ListJoin => eval_list_join(base, &eval_args(args, ctx)?),
        MethodId::ListCompact => eval_list_compact(base),
        MethodId::ListTranspose => eval_list_transpose(base),
        MethodId::ListSum | MethodId::ListAvg | MethodId::ListMax | MethodId::ListMin => {
            eval_reduce(
                ReduceKind::from_method(method).expect("reduce method id has a reduce kind"),
                base,
                ctx,
                span,
            )
        }
        MethodId::ListSumOver
        | MethodId::ListAvgOver
        | MethodId::ListMaxBy
        | MethodId::ListMinBy => eval_reduce_over(
            KeyedReduceKind::from_method(method).expect("reduce method id has a keyed reduce kind"),
            base,
            args,
            ctx,
            span,
        ),
        MethodId::OptionalIsSome
        | MethodId::OptionalIsNone
        | MethodId::OptionalUnwrap
        | MethodId::OptionalUnwrapOr => eval_optional(
            OptMethodKind::from_method(method).expect("optional method id has an optional kind"),
            base,
            args,
            ctx,
            span,
        ),
        MethodId::OptionalMap => eval_optional_map(base, args, ctx),
        MethodId::OptionalFlatten => eval_optional_flatten(base),
        MethodId::ListFlatten => eval_list_flatten(base),
        MethodId::ListExtend => eval_list_extend(base, eval_args(args, ctx)?),
        MethodId::ListTake | MethodId::ListSkip | MethodId::ListFirst | MethodId::ListNth => {
            eval_list_slice(method, base, &eval_args(args, ctx)?, span)
        }
        MethodId::ListLen => eval_list_len(base),
        MethodId::ListContains => {
            let mut arg_vals = eval_args(args, ctx)?;
            let Some(needle) = arg_vals.pop() else {
                return Err(ExecError::Integrity {
                    message: "`contains()` expects one argument (internal error)".to_string(),
                });
            };
            eval_list_contains(ctx, base, needle)
        }
        MethodId::StringSplit => eval_string_split(&base, &eval_args(args, ctx)?),
        _ if methods::method(method).is_scalar_method() => {
            let arg_vals = eval_args(args, ctx)?;
            eval_scalar_method(method, name, &base, &arg_vals, &ctx.schema.config, span)
                .map(Val::Scalar)
        }
        _ => Err(ExecError::Integrity {
            message: format!("unknown method `{name}` (internal error)"),
        }),
    };

    Ok(rewrap_optional(opt_depth, result?))
}

fn eval_args(args: &[TypedMethodArg], ctx: &mut EvalCtx<'_>) -> Result<Vec<Val>, ExecError> {
    args.iter()
        .map(|arg| eval(&arg.expr, ctx))
        .collect::<Result<Vec<_>, _>>()
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
