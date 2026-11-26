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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval::harness::{int, run_seeded, run_src, some, str};

    #[test]
    fn deep_optional_field_access() {
        assert_eq!(run_src("some(some({a = 1}))??.a"), Ok(some(some(int(1)))));
        assert_eq!(
            run_src("some(some({a = some(1)}))??.a"),
            Ok(some(some(some(int(1)))))
        );
        assert_eq!(
            run_src("some(if true {none} else {some({a = 1})})??.a"),
            Ok(some(Val::None))
        );
    }

    #[test]
    fn optional_field_access_nested_optional_preserved() {
        assert_eq!(
            run_src("some({a=if true {none} else {some(1)}})?.a"),
            Ok(some(Val::None))
        );
        assert_eq!(run_src("some({a=some(1)})?.a"), Ok(some(some(int(1)))));
    }

    #[test]
    fn optional_tuple_and_method_nested_optional_preserved() {
        assert_eq!(
            run_src("some((if true {none} else {some(1)}, 2))?.0"),
            Ok(some(Val::None))
        );
        assert_eq!(run_src("some([1, 2])?.nth(5)"), Ok(some(Val::None)));
        assert_eq!(run_src("some([1, 2])?.nth(0)"), Ok(some(some(int(1)))));
    }

    #[test]
    fn field_access_ref_base_resolved() {
        match run_seeded("users { o = orders { n = user.name } }") {
            Ok(Val::List(rows)) => {
                assert_eq!(rows.len(), 5);
                let Val::Record(first) = &rows[0] else {
                    panic!("expected record");
                };
                match first.iter().find(|(name, _)| name.as_ref() == "o") {
                    Some((_, Val::List(orders))) => {
                        assert_eq!(orders.len(), 2);
                        for order in orders {
                            let Val::Record(order) = order else {
                                panic!("expected order record, got {order}");
                            };
                            assert!(order.iter().any(|(name, value)| {
                                name.as_ref() == "n" && *value == str("Ada")
                            }));
                        }
                    }
                    got => panic!("expected orders list, got {got:?}"),
                }
            }
            got => panic!("expected projected users, got {got:?}"),
        }
    }

    #[test]
    fn field_access_optional_ref_base_resolved() {
        match run_seeded("users { m = manager?.name }") {
            Ok(Val::List(rows)) => {
                let Val::Record(first) = &rows[0] else {
                    panic!("expected record");
                };
                match first.iter().find(|(name, _)| name.as_ref() == "m") {
                    Some((_, Val::Some(name))) => {
                        assert!(matches!(name.as_ref(), Val::Scalar(_)));
                    }
                    got => panic!("expected some(name), got {got:?}"),
                }
                assert!(matches!(
                    &rows[1], Val::Record(second)
                      if matches!(second.first().unwrap().1, Val::None)
                ));
            }
            got => panic!("expected projected records, got {got:?}"),
        }
    }

    #[test]
    fn field_unowned_shallow_ref_resolution() {
        match run_seeded("users.manager") {
            Ok(Val::List(rows)) => match &rows[0] {
                Val::Some(user) => {
                    let Val::Record(fields) = user.as_ref() else {
                        panic!("expected manager record");
                    };
                    assert!(fields.iter().any(|(name, value)| {
                        name.as_ref() == "manager" && matches!(value, Val::Ref { .. })
                    }));
                }
                other => panic!("expected some(manager), got {other}"),
            },
            got => panic!("expected manager list, got {got:?}"),
        }
    }

    #[test]
    fn field_optional_owned_shallow_ref_resolution() {
        match run_seeded("users.profile") {
            Ok(Val::List(rows)) => {
                assert_eq!(rows.len(), 5);
                match &rows[0] {
                    Val::Some(profile) => {
                        let Val::Record(profile) = profile.as_ref() else {
                            panic!("expected profile record");
                        };
                        assert!(profile.iter().any(|(name, value)| {
                            name.as_ref() == "user" && matches!(value, Val::Ref { .. })
                        }));
                        assert!(profile.iter().any(|(name, value)| {
                            name.as_ref() == "bio" && *value == str("AdaBio")
                        }));
                    }
                    other => panic!("expected some(profile), got {other}"),
                }
                assert!(matches!(&rows[1], Val::None));
            }
            got => panic!("expected profile list, got {got:?}"),
        }
    }
}
