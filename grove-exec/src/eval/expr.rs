use grove_query::{IdentBinding, TypedExpr, TypedExprKind, TypedLetStmt};

use super::composite::{eval_array, eval_projection, eval_struct, eval_tuple};
use super::context::EvalCtx;
use super::data::{
    exec_field, exec_root, resolve_base_link, resolve_ident_value, resolve_owning_values,
};
use super::methods::eval_method;
use super::operators::{eval_binary, eval_cast, eval_unary};
use crate::error::ExecError;
use crate::value::literal::{decode_literal, eval_type_constant};
use crate::value::{Scalar, Val, rewrap_optional, unwrap_optional_depth};

pub(crate) fn eval(expr: &TypedExpr, ctx: &mut EvalCtx<'_>) -> Result<Val, ExecError> {
    match &expr.kind {
        TypedExprKind::Literal(lit) => Ok(decode_literal(lit, ctx.now)),
        TypedExprKind::Ident {
            name,
            binding,
            depth,
        } => {
            match binding {
                IdentBinding::Let => {
                    ctx.env
                        .lookup(name, *depth)
                        .cloned()
                        .ok_or_else(|| ExecError::Internal {
                            message: format!("unbound variable `{}`", name.value),
                        })
                }
                IdentBinding::Root { root_idx } => exec_root(ctx, *root_idx, expr.id),
                IdentBinding::Field(field) => {
                    let value = ctx.env.lookup(name, *depth).cloned().ok_or_else(|| {
                        ExecError::Internal {
                            message: format!("unbound field `{}`", name.value),
                        }
                    })?;
                    resolve_ident_value(ctx, expr.id, *field, value)
                }
                IdentBinding::ProjectedField { .. } => ctx
                    .env
                    .lookup(name, *depth)
                    .cloned()
                    .ok_or_else(|| ExecError::Internal {
                        message: format!("unbound field `{}`", name.value),
                    }),
                IdentBinding::ListElement => {
                    ctx.env
                        .lookup(name, *depth)
                        .cloned()
                        .ok_or_else(|| ExecError::Internal {
                            message: format!("unbound element `{}`", name.value),
                        })
                }
                IdentBinding::Prev { .. } => {
                    ctx.env
                        .lookup(name, *depth)
                        .cloned()
                        .ok_or_else(|| ExecError::Internal {
                            message: format!("unbound `prev` for `{}`", name.value),
                        })
                }
            }
        }
        TypedExprKind::EnumVariant { enum_id, variant } => Ok(Val::Enum(*enum_id, *variant)),
        TypedExprKind::Some { value } => eval(value, ctx).map(|v| Val::Some(Box::new(v))),
        TypedExprKind::TypeConstant { ty, name } => Ok(eval_type_constant(ty.value, name.value)),
        TypedExprKind::If { arms, default } => {
            for (cond, value) in arms {
                match eval(cond, ctx)? {
                    Val::Scalar(Scalar::Bool(true)) => return eval(value, ctx),
                    Val::Scalar(Scalar::Bool(false)) => {}
                    _ => {
                        return Err(ExecError::Internal {
                            message: "if condition must be a Bool".to_string(),
                        });
                    }
                }
            }
            eval(default, ctx)
        }
        TypedExprKind::Binary { op, lhs, rhs } => eval_binary(op, lhs, rhs, ctx, expr.span),
        TypedExprKind::Unary { op, expr: inner } => eval_unary(op, inner, ctx, expr.span),
        TypedExprKind::RowRef { expr: inner } => {
            let value = eval(inner, ctx)?;
            eval_row_ref(value)
        }
        TypedExprKind::Field {
            base,
            name,
            opt_depth,
        } => exec_field(base, name, *opt_depth, expr.id, ctx),
        TypedExprKind::TupleAccess {
            base,
            index,
            opt_depth,
        } => eval_tuple_access(base, *index, *opt_depth, ctx),
        TypedExprKind::Method { .. } => eval_method(expr, ctx),
        TypedExprKind::Cast { expr: inner, kind } => {
            let value = eval(inner, ctx)?;
            eval_cast(*kind, value, expr.span, ctx.schema)
        }
        TypedExprKind::Tuple { elements } => eval_tuple(elements, ctx),
        TypedExprKind::Array { elements } => eval_array(elements, ctx),
        TypedExprKind::Struct { fields } => eval_struct(fields, ctx),
        TypedExprKind::Projection { base, items } => eval_projection(base, items, ctx),
    }
}

/// Convert a traceable value into row references, preserving list/optional shape
fn eval_row_ref(value: Val) -> Result<Val, ExecError> {
    match value {
        Val::Record(record) => record
            .origin
            .map(Val::RowRef)
            .ok_or_else(|| ExecError::Internal {
                message: "row reference source record has no row origin".to_string(),
            }),
        Val::Some(inner) => eval_row_ref(*inner).map(|value| Val::Some(Box::new(value))),
        Val::None => Ok(Val::None),
        Val::List(list) => list
            .items
            .into_iter()
            .map(eval_row_ref)
            .collect::<Result<Vec<_>, _>>()
            .map(Val::list),
        other => Err(ExecError::Internal {
            message: format!("row reference source is not a record, list, or optional: {other}"),
        }),
    }
}

/// Evaluate 0-based tuple element access
fn eval_tuple_access(
    base: &TypedExpr,
    index: usize,
    opt_depth: usize,
    ctx: &mut EvalCtx<'_>,
) -> Result<Val, ExecError> {
    let base = eval(base, ctx)?;
    let base = resolve_base_link(ctx, base)?;
    let (base, peeled) = unwrap_optional_depth(base, opt_depth)?;
    if peeled < opt_depth {
        return Ok(rewrap_optional(peeled, Val::None));
    }
    let element = match base {
        Val::Tuple(elements) => {
            elements
                .into_iter()
                .nth(index)
                .ok_or_else(|| ExecError::Internal {
                    message: format!("tuple index {index} out of range"),
                })?
        }
        other => {
            return Err(ExecError::Internal {
                message: format!("tuple access on non-tuple value: {other}"),
            });
        }
    };
    Ok(rewrap_optional(opt_depth, element))
}

/// Evaluate a `let` and bind its value
pub(crate) fn eval_let(stmt: &TypedLetStmt, ctx: &mut EvalCtx<'_>) -> Result<(), ExecError> {
    let mut value = eval(&stmt.value, ctx)?;
    resolve_owning_values(ctx, std::slice::from_mut(&mut value), None)?;
    ctx.env.define(ctx.interner.intern(&stmt.name.value), value);
    Ok(())
}
