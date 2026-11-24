use grove_query::TypedMethodArg;
use grove_query::methods::{self, MethodId};
use grove_types::Span;

use super::{pop_element_scope, push_element_scope};
use crate::error::ExecError;
use crate::eval::data::{owning_refs_in, prefetch_owning, resolve_owning_values};
use crate::eval::{EvalCtx, eval};
use crate::value::{Scalar, Val};

pub(super) fn eval_list_slice(
    id: MethodId,
    base: Val,
    args: &[Val],
    span: Span,
) -> Result<Val, ExecError> {
    let name = methods::method(id)
        .names()
        .first()
        .copied()
        .unwrap_or("<list-method>");
    let mut items = match base {
        Val::List(items) => items,
        other => {
            return Err(ExecError::Internal {
                message: format!("`{name}()` on non-list value: {other}"),
            });
        }
    };
    let index_arg = || -> Result<i64, ExecError> {
        let [arg] = args else {
            return Err(ExecError::Internal {
                message: format!("`{name}()` expects one argument"),
            });
        };
        match arg {
            Val::Scalar(Scalar::Int(n)) => Ok(*n),
            _ => Err(ExecError::Internal {
                message: format!("`{name}()` expects an Int argument"),
            }),
        }
    };
    let result = match id {
        // take/skip: negative bounds abort
        MethodId::ListTake | MethodId::ListSkip => {
            let n = index_arg()?;
            if n < 0 {
                return Err(ExecError::Abort {
                    message: format!("`{name}()` called with negative bound {n}"),
                    span,
                });
            }
            let n = (n as usize).min(items.len());
            if id == MethodId::ListTake {
                items.truncate(n);
            } else {
                items.drain(..n);
            }
            Val::List(items)
        }
        MethodId::ListTakeBack | MethodId::ListSkipBack => {
            let n = index_arg()?;
            if n < 0 {
                return Err(ExecError::Abort {
                    message: format!("`{name}()` called with negative bound {n}"),
                    span,
                });
            }
            let n = (n as usize).min(items.len());
            if id == MethodId::ListTakeBack {
                items.drain(..items.len() - n);
            } else {
                items.truncate(items.len() - n);
            }
            Val::List(items)
        }
        MethodId::ListLast => match items.pop() {
            Some(last) => Val::Some(Box::new(last)),
            None => Val::None,
        },
        MethodId::ListFirst => match items.into_iter().next() {
            Some(head) => Val::Some(Box::new(head)),
            None => Val::None,
        },
        // nth: 0-based, negative indices from back
        MethodId::ListNth => {
            let n = index_arg()?;
            let len = items.len() as i64;
            let index = if n >= 0 { n } else { len + n };
            if index < 0 || index >= len {
                Val::None
            } else {
                Val::Some(Box::new(items.swap_remove(index as usize)))
            }
        }
        _ => {
            return Err(ExecError::Internal {
                message: format!("unknown slice method `{name}`"),
            });
        }
    };
    Ok(result)
}

/// Element count of a List
pub(super) fn eval_list_len(base: Val) -> Result<Val, ExecError> {
    let Val::List(items) = base else {
        return Err(ExecError::Internal {
            message: format!("`.len()` on non-list value: {base}"),
        });
    };
    Ok(Val::Scalar(Scalar::Int(items.len() as i64)))
}

/// List membership by element equality
pub(super) fn eval_list_contains(
    ctx: &mut EvalCtx<'_>,
    base: Val,
    needle: Val,
) -> Result<Val, ExecError> {
    let mut operands = [base, needle];
    resolve_owning_values(ctx, &mut operands, None)?;
    let [base, needle] = operands;
    let Val::List(items) = base else {
        return Err(ExecError::Internal {
            message: format!("`.contains()` on non-list value: {base}"),
        });
    };
    Ok(Val::Scalar(Scalar::Bool(
        items.iter().any(|item| item.eq(&needle) == Some(true)),
    )))
}

/// Join a list of strings with a separator
pub(super) fn eval_list_join(base: Val, args: &[Val]) -> Result<Val, ExecError> {
    let Val::List(items) = base else {
        return Err(ExecError::Internal {
            message: format!("`join()` on non-list value: {base}"),
        });
    };
    let separator = match args {
        [Val::Scalar(Scalar::String(separator))] => separator,
        _ => {
            return Err(ExecError::Internal {
                message: "`join()` expects one String argument".to_string(),
            });
        }
    };
    let mut parts = Vec::with_capacity(items.len());
    for item in items {
        let Val::Scalar(Scalar::String(part)) = item else {
            return Err(ExecError::Internal {
                message: format!("`join()` over non-String element: {item}"),
            });
        };
        parts.push(part);
    }
    Ok(Val::Scalar(Scalar::String(parts.join(separator))))
}

/// Drop `none` values from a List<?T>
pub(super) fn eval_list_compact(base: Val) -> Result<Val, ExecError> {
    let Val::List(items) = base else {
        return Err(ExecError::Internal {
            message: format!("`compact()` on non-list value: {base}"),
        });
    };
    let origin = items.origin;
    let mut compacted = Vec::with_capacity(items.len());
    for item in items {
        match item {
            Val::Some(value) => compacted.push(*value),
            Val::None => {}
            other => {
                return Err(ExecError::Internal {
                    message: format!("`compact()` over non-optional element: {other}"),
                });
            }
        }
    }
    Ok(Val::list_with_origin(compacted, origin))
}

/// Turn List<?T> into ?List<T>
pub(super) fn eval_list_transpose(base: Val) -> Result<Val, ExecError> {
    let Val::List(items) = base else {
        return Err(ExecError::Internal {
            message: format!("`transpose()` on non-list value: {base}"),
        });
    };
    let origin = items.origin;
    let mut transposed = Vec::with_capacity(items.len());
    for item in items {
        match item {
            Val::Some(value) => transposed.push(*value),
            Val::None => return Ok(Val::None),
            other => {
                return Err(ExecError::Internal {
                    message: format!("`transpose()` over non-optional element: {other}"),
                });
            }
        }
    }
    Ok(Val::Some(Box::new(Val::list_with_origin(
        transposed, origin,
    ))))
}

pub(super) fn eval_list_unique(base: Val) -> Result<Val, ExecError> {
    let Val::List(items) = base else {
        return Err(ExecError::Internal {
            message: format!("`unique()` on non-list value: {base}"),
        });
    };
    let origin = items.origin;
    let mut unique: Vec<Val> = Vec::with_capacity(items.len());
    for item in items {
        if !unique.iter().any(|seen| seen.eq(&item) == Some(true)) {
            unique.push(item);
        }
    }
    Ok(Val::list_with_origin(unique, origin))
}

pub(super) fn eval_list_unique_over(
    base: Val,
    args: &[TypedMethodArg],
    ctx: &mut EvalCtx<'_>,
) -> Result<Val, ExecError> {
    let Val::List(mut items) = base else {
        return Err(ExecError::Internal {
            message: format!("`unique_over()` on non-list value: {base}"),
        });
    };
    let origin = items.origin;
    let [arg] = args else {
        return Err(ExecError::Internal {
            message: "`unique_over()` expects one argument".to_string(),
        });
    };
    let fields = owning_refs_in(&arg.expr, ctx.schema, ctx.plan.prefetch_skip());
    prefetch_owning(ctx, &mut items, &fields)?;
    let mut unique: Vec<Val> = Vec::with_capacity(items.len());
    let mut keys: Vec<Val> = Vec::with_capacity(items.len());
    for item in items {
        let map_element = push_element_scope(ctx, &item);
        let key = eval(&arg.expr, ctx);
        pop_element_scope(ctx, map_element);
        let key = key?;
        if !keys.iter().any(|seen| seen.eq(&key) == Some(true)) {
            keys.push(key);
            unique.push(item);
        }
    }
    Ok(Val::list_with_origin(unique, origin))
}

pub(super) fn eval_list_is_unique(base: Val) -> Result<Val, ExecError> {
    let Val::List(items) = base else {
        return Err(ExecError::Internal {
            message: format!("`is_unique()` on non-list value: {base}"),
        });
    };
    for (index, item) in items.iter().enumerate() {
        if items[index + 1..]
            .iter()
            .any(|other| item.eq(other) == Some(true))
        {
            return Ok(Val::Scalar(Scalar::Bool(false)));
        }
    }
    Ok(Val::Scalar(Scalar::Bool(true)))
}

pub(super) fn eval_list_reverse(base: Val) -> Result<Val, ExecError> {
    let Val::List(mut items) = base else {
        return Err(ExecError::Internal {
            message: format!("`reverse()` on non-list value: {base}"),
        });
    };
    items.reverse();
    Ok(Val::List(items))
}

pub(super) fn eval_list_any(
    base: Val,
    args: &[TypedMethodArg],
    ctx: &mut EvalCtx<'_>,
) -> Result<Val, ExecError> {
    let items = predicate_items("any", base, args, ctx)?;
    let [arg] = args else {
        unreachable!("predicate_items validates the argument count");
    };
    for item in items {
        if eval_scoped_predicate(ctx, arg, &item)? {
            return Ok(Val::Scalar(Scalar::Bool(true)));
        }
    }
    Ok(Val::Scalar(Scalar::Bool(false)))
}

pub(super) fn eval_list_all(
    base: Val,
    args: &[TypedMethodArg],
    ctx: &mut EvalCtx<'_>,
) -> Result<Val, ExecError> {
    let items = predicate_items("all", base, args, ctx)?;
    let [arg] = args else {
        unreachable!("predicate_items validates the argument count");
    };
    for item in items {
        if !eval_scoped_predicate(ctx, arg, &item)? {
            return Ok(Val::Scalar(Scalar::Bool(false)));
        }
    }
    Ok(Val::Scalar(Scalar::Bool(true)))
}

pub(super) fn eval_list_find(
    base: Val,
    args: &[TypedMethodArg],
    ctx: &mut EvalCtx<'_>,
) -> Result<Val, ExecError> {
    let items = predicate_items("find", base, args, ctx)?;
    let [arg] = args else {
        unreachable!("predicate_items validates the argument count");
    };
    for item in items {
        if eval_scoped_predicate(ctx, arg, &item)? {
            return Ok(Val::Some(Box::new(item)));
        }
    }
    Ok(Val::None)
}

fn predicate_items(
    method: &str,
    base: Val,
    args: &[TypedMethodArg],
    ctx: &mut EvalCtx<'_>,
) -> Result<Vec<Val>, ExecError> {
    let Val::List(mut items) = base else {
        return Err(ExecError::Internal {
            message: format!("`{method}()` on non-list value: {base}"),
        });
    };
    let [arg] = args else {
        return Err(ExecError::Internal {
            message: format!("`{method}()` expects one argument"),
        });
    };
    let fields = owning_refs_in(&arg.expr, ctx.schema, ctx.plan.prefetch_skip());
    prefetch_owning(ctx, &mut items, &fields)?;
    Ok(items.items)
}

fn eval_scoped_predicate(
    ctx: &mut EvalCtx<'_>,
    arg: &TypedMethodArg,
    item: &Val,
) -> Result<bool, ExecError> {
    let map_element = push_element_scope(ctx, item);
    let pred = eval(&arg.expr, ctx);
    pop_element_scope(ctx, map_element);
    match pred? {
        Val::Scalar(Scalar::Bool(value)) => Ok(value),
        other => Err(ExecError::Internal {
            message: format!("predicate evaluated to non-Bool: {other}"),
        }),
    }
}

pub(super) fn eval_list_flatten(base: Val) -> Result<Val, ExecError> {
    let Val::List(items) = base else {
        return Err(ExecError::Internal {
            message: format!("`flatten()` on non-list value: {base}"),
        });
    };
    let mut flat = Vec::new();
    for item in items {
        match item {
            Val::List(inner) => flat.extend(inner),
            other => {
                return Err(ExecError::Internal {
                    message: format!("`flatten()` over non-list element: {other}"),
                });
            }
        }
    }
    Ok(Val::list(flat))
}

/// Concatenate two compatible lists
pub(super) fn eval_list_extend(base: Val, mut args: Vec<Val>) -> Result<Val, ExecError> {
    let mut items = match base {
        Val::List(items) => items,
        other => {
            return Err(ExecError::Internal {
                message: format!("`extend()` on non-list value: {other}"),
            });
        }
    };
    let Some(Val::List(other)) = args.pop() else {
        return Err(ExecError::Internal {
            message: "`extend()` expects one list argument".to_string(),
        });
    };
    items.extend(other);
    Ok(Val::list(items.items))
}

#[cfg(test)]
mod tests {
    use crate::error::ExecError;
    use crate::eval::harness::*;
    use crate::value::{Scalar, Val};

    #[test]
    fn method_flatten() {
        match run_seeded("users.orders.flatten()") {
            Ok(Val::List(orders)) => {
                let amounts: Vec<i64> = orders
                    .iter()
                    .map(|order| match order {
                        Val::Record(fields) => {
                            match fields.iter().find(|(name, _)| name.as_ref() == "amount") {
                                Some((_, Val::Scalar(Scalar::Int(n)))) => *n,
                                got => panic!("expected amount, got {got:?}"),
                            }
                        }
                        other => panic!("expected order record, got {other}"),
                    })
                    .collect();
                assert_eq!(amounts, [100, 200, 400]);
            }
            got => panic!("expected flat orders, got {got:?}"),
        }
    }

    #[test]
    fn method_extend() {
        assert_eq!(
            run_src("[0, -2, 3].extend([4, -6])"),
            Ok(Val::list(vec![int(0), int(-2), int(3), int(4), int(-6)])),
        );
        assert_eq!(run_src("[\"\"].skip(1).extend([])"), Ok(Val::list(vec![])),);
        assert_eq!(
            run_seeded("users.first()?.orders?.extend(users.nth(1)?.orders.unwrap_or([]))?.amount",),
            Ok(some(Val::list(vec![int(100), int(200), int(400)]))),
        );
        assert_eq!(
            run_seeded("users.extend(users).len() == users.len() * 2"),
            Ok(bool(true)),
        );
    }

    #[test]
    fn method_take_skip() {
        let names = |src: &str| match run_seeded(src) {
            Ok(Val::List(rows)) => rows
                .into_iter()
                .map(|row| match row {
                    Val::Record(fields) => fields
                        .into_iter()
                        .find_map(|(name, value)| (name.as_ref() == "name").then_some(value))
                        .expect("expected `name` field"),
                    other => panic!("expected record, got {other}"),
                })
                .collect::<Vec<_>>(),
            got => panic!("expected rows for {src}, got {got:?}"),
        };
        assert_eq!(names("users.take(2)"), [str("Ada"), str("Bob")]);
        assert_eq!(names("users.skip(3)"), [str("Dan"), str("Eli")]);
        assert_eq!(names("users.take(0)"), Vec::new());
        assert_eq!(names("users.skip(5)"), Vec::new());
        assert_eq!(names("users.take(99)").len(), 5);
        assert_eq!(names("users.skip(99)"), Vec::new());
        assert_eq!(names("users.skip(1).take(2)"), [str("Bob"), str("Cyd")]);
        assert_eq!(names("users.take(5).skip(3)"), [str("Dan"), str("Eli")]);
        assert_eq!(names("users.take(2).take(5)").len(), 2);
        assert_eq!(names("users.skip(1).skip(2)").len(), 2);
        match run_seeded("users { o = orders.take(1) }") {
            Ok(Val::List(rows)) => {
                let counts: Vec<usize> = rows
                    .iter()
                    .map(|row| match row {
                        Val::Record(fields) => {
                            match fields.iter().find(|(name, _)| name.as_ref() == "o") {
                                Some((_, Val::List(orders))) => orders.len(),
                                got => panic!("expected orders list, got {got:?}"),
                            }
                        }
                        other => panic!("expected record, got {other}"),
                    })
                    .collect();
                assert_eq!(counts, [1, 1, 0, 0, 0]);
            }
            got => panic!("expected projected users, got {got:?}"),
        }
        match run_seeded("users.take(-1)") {
            Err(ExecError::Abort { message, .. }) => {
                assert!(message.contains("negative bound"), "got: {message}")
            }
            got => panic!("expected abort, got {got:?}"),
        }
        match run_seeded("users.skip(-2)") {
            Err(ExecError::Abort { .. }) => {}
            got => panic!("expected abort, got {got:?}"),
        }
    }

    #[test]
    fn method_first_nth() {
        let nth_name = |src: &str| match run_seeded(src) {
            Ok(Val::Some(user)) => match *user {
                Val::Record(user) => user
                    .into_iter()
                    .find_map(|(name, value)| (name.as_ref() == "name").then_some(value)),
                other => panic!("expected user record, got {other}"),
            },
            Ok(Val::None) => None,
            got => panic!("expected optional user for {src}, got {got:?}"),
        };
        assert_eq!(nth_name("users.nth(0)"), Some(str("Ada")));
        assert_eq!(nth_name("users.nth(4)"), Some(str("Eli")));
        assert_eq!(nth_name("users.nth(-1)"), Some(str("Eli")));
        assert_eq!(nth_name("users.nth(-5)"), Some(str("Ada")));
        assert_eq!(nth_name("users.nth(5)"), None);
        assert_eq!(nth_name("users.nth(-6)"), None);
        assert_eq!(nth_name("users.skip(5).nth(0)"), None);
        assert_eq!(nth_name("users.skip(5).nth(-1)"), None);
        assert_eq!(nth_name("users.take(2).first()"), Some(str("Ada")));
        assert_eq!(nth_name("users.skip(5).first()"), None);
        assert_eq!(nth_name("users.skip(2).first()"), Some(str("Cyd")));
        assert_eq!(nth_name("users.skip(2).nth(-1)"), Some(str("Eli")));
        assert_eq!(nth_name("users.take(3).nth(2)"), Some(str("Cyd")));
    }

    #[test]
    fn method_list_len() {
        assert_eq!(run_seeded("users.len()"), Ok(int(5)));
        assert_eq!(run_seeded("roles.len()"), Ok(int(2)));
        assert_eq!(run_seeded("users.take(2).len()"), Ok(int(2)));
        assert_eq!(run_seeded("users.skip(5).len()"), Ok(int(0)));
        match run_seeded("users { n = orders.len() }") {
            Ok(Val::List(rows)) => {
                let counts: Vec<i64> = rows
                    .iter()
                    .map(|row| match row {
                        Val::Record(fields) => {
                            match fields.iter().find(|(name, _)| name.as_ref() == "n") {
                                Some((_, Val::Scalar(Scalar::Int(n)))) => *n,
                                got => panic!("expected count, got {got:?}"),
                            }
                        }
                        other => panic!("expected record, got {other}"),
                    })
                    .collect();
                assert_eq!(counts, [2, 1, 0, 0, 0]);
            }
            got => panic!("expected projected users, got {got:?}"),
        }
        assert_eq!(run_seeded("users.roles.len()"), Ok(int(5)));
        assert_eq!(run_seeded("users.roles.flatten().len()"), Ok(int(4)));
    }

    #[test]
    fn method_join() {
        assert_eq!(run_src(r#"["a", "b", "c"].join("-")"#), Ok(str("a-b-c")));
        assert_eq!(run_src(r#"["x"].skip(1).join(",")"#), Ok(str("")));
    }

    #[test]
    fn method_compact_and_transpose() {
        assert_eq!(
            run_src("[some(1), none, some(3)].compact()"),
            Ok(Val::list(vec![int(1), int(3)]))
        );
        assert_eq!(run_src("[some(1), none].transpose()"), Ok(Val::None));
        assert_eq!(
            run_src("[some(1), some(2)].transpose()"),
            Ok(some(Val::list(vec![int(1), int(2)])))
        );
        assert_eq!(
            run_src("[some(1)].skip(1).transpose()"),
            Ok(some(Val::list(Vec::new())))
        );
        assert_eq!(
            run_src("[some(some(1)), some(none)].transpose()"),
            Ok(some(Val::list(vec![some(int(1)), Val::None])))
        );
    }

    #[test]
    fn method_unique() {
        assert_eq!(
            run_src("[1, 2, 1, 3, 2].unique()"),
            Ok(Val::list(vec![int(1), int(2), int(3)]))
        );
        assert_eq!(
            run_src("[{ a = 1, b = 2 }, { a = 1, b = 3 }, { a = 2, b = 4 }].unique_over(a)"),
            Ok(Val::list(vec![
                rec(vec![("a", int(1)), ("b", int(2))]),
                rec(vec![("a", int(2)), ("b", int(4))]),
            ]))
        );
        assert_eq!(
            run_src("[1, 2, 3, 4].unique_over($ mod 2)"),
            Ok(Val::list(vec![int(1), int(2)]))
        );
        assert_eq!(run_src("[1, 2, 3].is_unique()"), Ok(bool(true)));
        assert_eq!(run_src("[1, 2, 1].is_unique()"), Ok(bool(false)));
        assert_eq!(run_src("[1].skip(1).is_unique()"), Ok(bool(true)));
    }

    #[test]
    fn method_last_and_reverse() {
        assert_eq!(run_src("[1, 2, 3].last()"), Ok(some(int(3))));
        assert_eq!(run_src("[1].skip(1).last()"), Ok(Val::None));
        assert_eq!(
            run_src("[1, 2, 3].reverse()"),
            Ok(Val::list(vec![int(3), int(2), int(1)]))
        );
        assert_eq!(run_src("[1, 2, 3].last() == some(3)"), Ok(bool(true)));
    }

    #[test]
    fn method_any_all_find() {
        assert_eq!(run_src("[1, 2, 3].any($ > 2)"), Ok(bool(true)));
        assert_eq!(run_src("[1, 2, 3].all($ > 2)"), Ok(bool(false)));
        assert_eq!(run_src("[1, 2, 3].all($ > 0)"), Ok(bool(true)));
        assert_eq!(run_src("[1, 2, 3].find($ > 1)"), Ok(some(int(2))));
        assert_eq!(run_src("[1, 2, 3].find($ > 9)"), Ok(Val::None));
        assert_eq!(run_src("[1].skip(1).any($ > 0)"), Ok(bool(false)));
        assert_eq!(run_src("[1].skip(1).all($ > 0)"), Ok(bool(true)));
        assert_eq!(run_src("[1].skip(1).find($ > 0)"), Ok(Val::None));
        assert_eq!(run_seeded("users.any(active)"), Ok(bool(true)));
        assert_eq!(run_seeded("users.all(active)"), Ok(bool(false)));
        assert_eq!(
            run_seeded("users.find(name == \"Cyd\")?.name"),
            Ok(some(str("Cyd")))
        );
        assert_eq!(run_seeded("users.find(name == \"Nobody\")"), Ok(Val::None));
    }

    #[test]
    fn method_back_slices() {
        assert_eq!(
            run_src("[1, 2, 3].take_back(2)"),
            Ok(Val::list(vec![int(2), int(3)]))
        );
        assert_eq!(run_src("[1, 2, 3].take_back(0)"), Ok(Val::list(vec![])));
        assert_eq!(
            run_src("[1, 2, 3].take_back(9)"),
            Ok(Val::list(vec![int(1), int(2), int(3)]))
        );
        assert_eq!(
            run_src("[1, 2, 3].skip_back(1)"),
            Ok(Val::list(vec![int(1), int(2)]))
        );
        assert_eq!(
            run_src("[1, 2, 3].skip_back(0)"),
            Ok(Val::list(vec![int(1), int(2), int(3)]))
        );
        assert_eq!(run_src("[1, 2, 3].skip_back(9)"), Ok(Val::list(vec![])));
        assert!(
            run_src("[1].take_back(-1)")
                .unwrap_err()
                .to_string()
                .contains("negative bound")
        );
        assert!(
            run_src("[1].skip_back(-1)")
                .unwrap_err()
                .to_string()
                .contains("negative bound")
        );
        assert_eq!(
            run_src("[1, 2, 3].reverse().take_back(2)"),
            Ok(Val::list(vec![int(2), int(1)]))
        );
    }

    #[test]
    fn method_pushdown_composition() {
        let names = |src: &str| match run_seeded(src) {
            Ok(Val::List(values)) => values,
            got => panic!("expected names for {src}, got {got:?}"),
        };
        assert_eq!(run_seeded("users.last()?.name"), Ok(some(str("Eli"))));
        assert_eq!(
            names("users.reverse().name"),
            vec![str("Eli"), str("Dan"), str("Cyd"), str("Bob"), str("Ada")]
        );
        assert_eq!(
            names("users.sort_over(id).reverse().name"),
            vec![str("Eli"), str("Dan"), str("Cyd"), str("Bob"), str("Ada")]
        );
        assert_eq!(
            names("users.reverse().sort_over(name).name"),
            vec![str("Ada"), str("Bob"), str("Cyd"), str("Dan"), str("Eli")]
        );
        assert_eq!(
            run_seeded("users.sort_desc_over(id).last()?.name"),
            Ok(some(str("Ada")))
        );
        assert_eq!(
            names("users.take_back(2).name"),
            vec![str("Dan"), str("Eli")]
        );
        assert_eq!(
            names("users.skip_back(2).name"),
            vec![str("Ada"), str("Bob"), str("Cyd")]
        );
        assert_eq!(
            names("users.take_back(3).take_back(2).name"),
            vec![str("Dan"), str("Eli")]
        );
        assert_eq!(
            names("users.take_back(3).skip_back(1).name"),
            vec![str("Cyd"), str("Dan")]
        );
        assert_eq!(
            names("users.reverse().take(2).name"),
            vec![str("Eli"), str("Dan")]
        );
        assert_eq!(
            names("users.take(2).reverse().name"),
            vec![str("Bob"), str("Ada")]
        );
        assert_eq!(
            run_seeded("users.take(2).last()?.name"),
            Ok(some(str("Bob")))
        );
        assert_eq!(
            run_seeded("users.skip(2).last()?.name"),
            Ok(some(str("Eli")))
        );
        assert_eq!(
            run_seeded("users.reverse().last()?.name"),
            Ok(some(str("Ada")))
        );
        assert_eq!(
            run_seeded("users.take_back(2).last()?.name"),
            Ok(some(str("Eli")))
        );
        assert_eq!(
            run_seeded("users.skip(2).any(name == \"Bob\")"),
            Ok(bool(false))
        );
        assert_eq!(run_seeded("users.take(0).any(active)"), Ok(bool(false)));
        assert_eq!(run_seeded("users.take(0).all(active)"), Ok(bool(true)));
        assert_eq!(
            run_seeded("users.take(1).find(name == \"Ada\")?.name"),
            Ok(some(str("Ada")))
        );
        assert_eq!(
            run_seeded("users.reverse().find(active)?.name"),
            Ok(some(str("Dan")))
        );
        assert_eq!(
            run_seeded("users[active].any(name == \"Ada\")"),
            Ok(bool(true))
        );
        assert_eq!(
            run_seeded("users[active].find(name == \"Cyd\")"),
            Ok(Val::None)
        );
        assert_eq!(
            run_seeded("users.first()?.orders?.reverse()?.amount"),
            Ok(some(Val::list(vec![int(200), int(100)])))
        );
    }

    #[test]
    fn method_contains() {
        for src in [
            "[1, 2, 3].contains(2)",
            "[\"a\", \"bb\"].contains(\"bb\")",
            "[(1, 2), (3, 4)].contains((3, 4))",
            "users.tags.flatten().contains(\"a1\")",
        ] {
            assert_eq!(run_seeded(src), Ok(bool(true)), "{src}");
        }
        for src in [
            "[1, 2, 3].contains(9)",
            "[(1, 2)].contains((1, 3))",
            "users.tags.flatten().contains(\"zzz\")",
        ] {
            assert_eq!(run_seeded(src), Ok(bool(false)), "{src}");
        }
        match run_seeded("users { t = tags.contains(\"a1\") }") {
            Ok(Val::List(rows)) => {
                let flags: Vec<Val> = rows
                    .iter()
                    .map(|row| match row {
                        Val::Record(fields) => {
                            match fields.iter().find(|(name, _)| name.as_ref() == "t") {
                                Some((_, value)) => value.clone(),
                                got => panic!("expected flag, got {got:?}"),
                            }
                        }
                        other => panic!("expected record, got {other}"),
                    })
                    .collect();
                assert_eq!(
                    flags,
                    [
                        bool(true),
                        bool(false),
                        bool(false),
                        bool(false),
                        bool(true)
                    ]
                );
            }
            got => panic!("expected projected users, got {got:?}"),
        }
        assert_eq!(
            run_seeded("users.scores.flatten().contains((1, \"one\"))"),
            Ok(bool(true))
        );
        assert_eq!(
            run_seeded("users.scores.flatten().contains((9, \"nine\"))"),
            Ok(bool(false))
        );
        assert_eq!(
            run_seeded("users.first()?.tags?.contains(\"a1\")"),
            Ok(some(bool(true)))
        );
        assert_eq!(
            run_seeded("users { id, name }.contains({ name = \"Ada\", id = 1 })"),
            Ok(bool(true))
        );
        assert_eq!(
            run_seeded("users { id, name }.contains({ id = 1, name = \"Ada\" })"),
            Ok(bool(true))
        );
        assert_eq!(
            run_seeded("users { id, name }.contains({ id = 9, name = \"A\" })"),
            Ok(bool(false))
        );
        assert_eq!(
            run_seeded("users.contains(users.first().unwrap())"),
            Ok(bool(true))
        );
    }
}
