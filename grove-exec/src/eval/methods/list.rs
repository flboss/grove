use grove_types::Span;

use crate::error::ExecError;
use crate::eval::{EvalCtx, data::resolve_owning_values};
use crate::value::{Scalar, Val};

pub(super) fn eval_slice(
    name: &str,
    base: Val,
    args: &[Val],
    span: Span,
) -> Result<Val, ExecError> {
    let mut items = match base {
        Val::List(items) => items,
        other => {
            return Err(ExecError::Integrity {
                message: format!("`{name}()` on non-list value: {other} (internal error)"),
            });
        }
    };
    let index_arg = |method: &str| -> Result<i64, ExecError> {
        let [arg] = args else {
            return Err(ExecError::Integrity {
                message: format!("`{method}()` expects one argument (internal error)"),
            });
        };
        match arg {
            Val::Scalar(Scalar::Int(n)) => Ok(*n),
            _ => Err(ExecError::Integrity {
                message: format!("`{method}()` expects an Int argument (internal error)"),
            }),
        }
    };
    let result = match name {
        // take/skip: negative bounds abort
        "take" | "skip" => {
            let n = index_arg(name)?;
            if n < 0 {
                return Err(ExecError::Abort {
                    message: format!("`{name}()` called with negative bound {n}"),
                    span,
                });
            }
            let n = (n as usize).min(items.len());
            if name == "take" {
                items.truncate(n);
            } else {
                items.drain(..n);
            }
            Val::List(items)
        }
        // first: head or None
        "first" => match items.into_iter().next() {
            Some(head) => Val::Some(Box::new(head)),
            None => Val::None,
        },
        // nth: 0-based, negative indices from back, out-of-range is None
        "nth" => {
            let n = index_arg(name)?;
            let len = items.len() as i64;
            let index = if n >= 0 { n } else { len + n };
            if index < 0 || index >= len {
                Val::None
            } else {
                Val::Some(Box::new(items.swap_remove(index as usize)))
            }
        }
        _ => {
            return Err(ExecError::Integrity {
                message: format!("unknown slice method `{name}` (internal error)"),
            });
        }
    };
    Ok(result)
}

/// Element count of a List
pub(super) fn eval_list_len(base: Val) -> Result<Val, ExecError> {
    let Val::List(items) = base else {
        return Err(ExecError::Integrity {
            message: format!("`.len()` on non-list value: {base} (internal error)"),
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
        return Err(ExecError::Integrity {
            message: format!("`.contains()` on non-list value: {base} (internal error)"),
        });
    };
    Ok(Val::Scalar(Scalar::Bool(
        items.iter().any(|item| item.eq(&needle) == Some(true)),
    )))
}

pub(super) fn eval_flatten(base: Val) -> Result<Val, ExecError> {
    let Val::List(items) = base else {
        return Err(ExecError::Integrity {
            message: format!("`flatten()` on non-list value: {base} (internal error)"),
        });
    };
    let mut flat = Vec::new();
    for item in items {
        match item {
            Val::List(inner) => flat.extend(inner),
            other => {
                return Err(ExecError::Integrity {
                    message: format!("`flatten()` over non-list element: {other} (internal error)"),
                });
            }
        }
    }
    Ok(Val::List(flat))
}

/// Concatenate two compatible lists
pub(super) fn eval_extend(base: Val, mut args: Vec<Val>) -> Result<Val, ExecError> {
    let mut items = match base {
        Val::List(items) => items,
        other => {
            return Err(ExecError::Integrity {
                message: format!("`extend()` on non-list value: {other} (internal error)"),
            });
        }
    };
    let Some(Val::List(other)) = args.pop() else {
        return Err(ExecError::Integrity {
            message: "`extend()` expects one list argument (internal error)".to_string(),
        });
    };
    items.extend(other);
    Ok(Val::List(items))
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
            Ok(Val::List(vec![int(0), int(-2), int(3), int(4), int(-6)])),
        );
        assert_eq!(run_src("[\"\"].skip(1).extend([])"), Ok(Val::List(vec![])),);
        assert_eq!(
            run_seeded("users.first()?.orders?.extend(users.nth(1)?.orders.unwrap_or([]))?.amount",),
            Ok(some(Val::List(vec![int(100), int(200), int(400)]))),
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
    }
}
