use std::rc::Rc;

use grove_query::TypedExpr;
use grove_types::NodeId;

use super::fetch::resolve_field;
use super::refs::resolve_refs;
use crate::error::ExecError;
use crate::eval::{EvalCtx, eval};
use crate::storage::FetchConstraints;
use crate::value::{Val, rewrap_optional, unwrap_optional_depth};

pub(crate) fn exec_field(
    base: &TypedExpr,
    name: &str,
    opt_depth: usize,
    node: NodeId,
    ctx: &mut EvalCtx,
) -> Result<Val, ExecError> {
    let base = eval(base, ctx)?;
    let base = resolve_base_link(ctx, base)?;
    let (base, peeled) = unwrap_optional_depth(base, opt_depth)?;
    if peeled < opt_depth {
        return Ok(rewrap_optional(peeled, Val::None));
    }
    let result = match base {
        Val::Record(fields) => {
            let value = record_field(fields, name)?;
            let constraints = ctx
                .plan
                .constraints_for(node)
                .filter(|constraints| !constraints.is_empty());
            match (constraints, value) {
                (Some(constraints), Val::Ref { field, parent }) => {
                    let mut resolved = resolve_field(ctx, field, &[parent], constraints)?;
                    ctx.applied_triggers.insert(node);
                    let value = match resolved.pop() {
                        Some((_, value)) => value,
                        None => {
                            return Err(ExecError::Internal {
                                message: "constrained Ref resolved to no value".to_string(),
                            });
                        }
                    };
                    return Ok(rewrap_optional(opt_depth, value));
                }
                (_, value) => value,
            }
        }
        Val::List(items) => {
            let origin = items.origin;
            let mut mapped = Vec::with_capacity(items.len());
            for item in items {
                match item {
                    Val::Record(fields) => mapped.push(record_field(fields, name)?),
                    Val::None => mapped.push(Val::None),
                    other => {
                        return Err(ExecError::Internal {
                            message: format!("field `{name}` on non-record list element: {other}"),
                        });
                    }
                }
            }
            Val::list_with_origin(mapped, origin)
        }
        _ => {
            return Err(ExecError::Internal {
                message: format!("field `{name}` on non-record value"),
            });
        }
    };
    let resolved = resolve_refs(ctx, result)?;
    Ok(rewrap_optional(opt_depth, resolved))
}

/// Resolve a bare Ref base before navigation
pub(crate) fn resolve_base_link(ctx: &mut EvalCtx, base: Val) -> Result<Val, ExecError> {
    match base {
        Val::Ref { field, parent } => {
            let mut resolved = resolve_field(ctx, field, &[parent], &FetchConstraints::EMPTY)?;
            match resolved.pop() {
                Some((_, value)) => Ok(value),
                None => Err(ExecError::Internal {
                    message: "base Ref resolved to no value".to_string(),
                }),
            }
        }
        Val::List(items) => {
            let origin = items.origin;
            let mut resolved_items = Vec::with_capacity(items.len());
            for item in items {
                resolved_items.push(resolve_base_link(ctx, item)?);
            }
            Ok(Val::list_with_origin(resolved_items, origin))
        }
        Val::Record(_)
        | Val::Some(_)
        | Val::None
        | Val::Scalar(_)
        | Val::Enum(_, _)
        | Val::Tuple(_)
        | Val::RowRef(_) => Ok(base),
    }
}

fn record_field(
    fields: impl IntoIterator<Item = (Rc<str>, Val)>,
    name: &str,
) -> Result<Val, ExecError> {
    fields
        .into_iter()
        .find(|(field_name, _)| field_name.as_ref() == name)
        .map(|(_, value)| value)
        .ok_or_else(|| ExecError::Internal {
            message: format!("unknown field `{name}`"),
        })
}
