use std::fmt;

use grove_query::typecheck::types::TypedMethodArg;
use grove_types::Span;

use super::super::{EvalCtx, eval};
use crate::error::ExecError;
use crate::value::{Scalar, Val};

#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum OptMethodKind {
    IsSome,
    IsNone,
    Unwrap,
    UnwrapOr,
}

impl OptMethodKind {
    pub(super) fn from_str(name: &str) -> Option<OptMethodKind> {
        Some(match name {
            "is_some" => OptMethodKind::IsSome,
            "is_none" => OptMethodKind::IsNone,
            "unwrap" => OptMethodKind::Unwrap,
            "unwrap_or" => OptMethodKind::UnwrapOr,
            _ => return None,
        })
    }
}

impl fmt::Display for OptMethodKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OptMethodKind::IsSome => write!(f, "is_some"),
            OptMethodKind::IsNone => write!(f, "is_none"),
            OptMethodKind::Unwrap => write!(f, "unwrap"),
            OptMethodKind::UnwrapOr => write!(f, "unwrap_or"),
        }
    }
}

/// Collapse or default an optional
pub(super) fn eval_optional(
    kind: OptMethodKind,
    base: Val,
    optional: bool,
    args: &[TypedMethodArg],
    ctx: &mut EvalCtx<'_>,
    span: Span,
) -> Result<Val, ExecError> {
    if !args.is_empty() && kind != OptMethodKind::UnwrapOr {
        return Err(ExecError::Integrity {
            message: format!("`{kind}()` takes no arguments (internal error)"),
        });
    }
    if kind == OptMethodKind::UnwrapOr && args.len() != 1 {
        return Err(ExecError::Integrity {
            message: "`unwrap_or()` expects one argument (internal error)".to_string(),
        });
    }
    // dispatch already unwraps outer optional on `?.`
    let result = match base {
        Val::None => eval_opt_absent(kind, args, ctx, span)?,
        Val::Some(v) => eval_opt_present(kind, *v),
        _ => {
            return Err(ExecError::Integrity {
                message: format!(
                    "`{kind}()` called on non-optional value: {base} (internal error)"
                ),
            });
        }
    };
    Ok(if optional {
        Val::Some(Box::new(result))
    } else {
        result
    })
}

fn eval_opt_present(kind: OptMethodKind, value: Val) -> Val {
    match kind {
        OptMethodKind::IsSome => Val::Scalar(Scalar::Bool(true)),
        OptMethodKind::IsNone => Val::Scalar(Scalar::Bool(false)),
        _ => value,
    }
}

fn eval_opt_absent(
    kind: OptMethodKind,
    args: &[TypedMethodArg],
    ctx: &mut EvalCtx<'_>,
    span: Span,
) -> Result<Val, ExecError> {
    match kind {
        OptMethodKind::IsSome => Ok(Val::Scalar(Scalar::Bool(false))),
        OptMethodKind::IsNone => Ok(Val::Scalar(Scalar::Bool(true))),
        OptMethodKind::Unwrap => Err(ExecError::Abort {
            message: "`unwrap()` called on `none`".to_string(),
            span,
        }),
        OptMethodKind::UnwrapOr => {
            let [arg] = args else {
                return Err(ExecError::Integrity {
                    message: "`unwrap_or()` expects one argument (internal error)".to_string(),
                });
            };
            Ok(eval_opt_present(kind, eval(&arg.expr, ctx)?))
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::error::ExecError;
    use crate::eval::harness::*;
    use crate::value::Val;

    #[test]
    fn method_optional_presence() {
        assert_eq!(run_src("some(42).is_some()"), Ok(bool(true)));
        assert_eq!(run_src("some(42).is_none()"), Ok(bool(false)));
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
        assert_eq!(
            names("users[manager.is_some()]"),
            [str("Ada"), str("Cyd"), str("Dan")]
        );
        assert_eq!(
            names("users[!!profile.is_none()]"),
            [str("Bob"), str("Dan"), str("Eli")]
        );
        assert_eq!(
            run_seeded("users.first()?.nickname.is_some()"),
            Ok(bool(true))
        );
        assert_eq!(
            run_seeded("users.skip(5).first()?.nickname.is_some()"),
            Ok(bool(false))
        );
    }

    #[test]
    fn method_unwrap() {
        assert_eq!(run_src("some(42).unwrap()"), Ok(int(42)));
        assert_eq!(
            run_seeded("users.skip(1).first().unwrap().name"),
            Ok(str("Bob"))
        );
        match run_seeded("users.skip(5).first().unwrap()") {
            Err(ExecError::Abort { message, .. }) => {
                assert!(message.contains("unwrap"), "got: {message}")
            }
            got => panic!("expected abort, got {got:?}"),
        }
        assert_eq!(run_src("some(some(1))?.unwrap()"), Ok(some(int(1))));
        assert_eq!(
            run_seeded("users.first()?.nickname?.is_some()"),
            Ok(some(bool(true)))
        );
    }

    #[test]
    fn method_unwrap_or() {
        assert_eq!(run_src("some(1).unwrap_or(99)"), Ok(int(1)));
        assert_eq!(
            run_src("some(some(1)).unwrap_or(some(99))"),
            Ok(some(int(1)))
        );
        assert_eq!(run_src("some(1).unwrap_or(1 / 0)"), Ok(int(1)));
        assert_eq!(run_src("some(some(1))?.unwrap_or(1 / 0)"), Ok(some(int(1))));
        assert_eq!(
            run_seeded("users.skip(1).first()?.nickname.unwrap_or(some(\"anon\"))"),
            Ok(Val::None)
        );
        assert_eq!(
            run_seeded("users.skip(1).first()?.nickname?.unwrap_or(\"anon\")"),
            Ok(some(str("anon")))
        );
        assert_eq!(
            run_seeded("users.first()?.nickname.unwrap_or(some(\"B\"))"),
            Ok(some(str("A")))
        );
        match run_seeded("users { n = nickname.unwrap_or(\"anon\") }") {
            Ok(Val::List(rows)) => {
                let nicknames: Vec<Val> = rows
                    .iter()
                    .map(|row| match row {
                        Val::Record(fields) => {
                            match fields.iter().find(|(name, _)| name.as_ref() == "n") {
                                Some((_, value)) => value.clone(),
                                got => panic!("expected nickname, got {got:?}"),
                            }
                        }
                        other => panic!("expected record, got {other}"),
                    })
                    .collect();
                assert_eq!(
                    nicknames,
                    [str("A"), str("anon"), str("C"), str("anon"), str("E")]
                );
            }
            got => panic!("expected projected users, got {got:?}"),
        }
        match run_seeded("users { m = manager?.name.unwrap_or(\"boss\") }") {
            Ok(Val::List(rows)) => {
                let names: Vec<Val> = rows
                    .iter()
                    .map(|row| match row {
                        Val::Record(fields) => {
                            match fields.iter().find(|(name, _)| name.as_ref() == "m") {
                                Some((_, value)) => value.clone(),
                                got => panic!("expected name, got {got:?}"),
                            }
                        }
                        other => panic!("expected record, got {other}"),
                    })
                    .collect();
                assert_eq!(
                    names,
                    [str("Bob"), str("boss"), str("Bob"), str("Ada"), str("boss")]
                );
            }
            got => panic!("expected projected users, got {got:?}"),
        }
        assert_eq!(
            run_seeded("users.skip(5).first()?.nickname.unwrap_or(some(\"anon\"))"),
            Ok(some(str("anon")))
        );
    }

    #[test]
    fn eval_optional_method_on_some() {
        assert_eq!(
            run_src(r#"some("hi")?.len()"#),
            Ok(Val::Some(Box::new(int(2))))
        );
        assert_eq!(
            run_src(r#"some("hi")?.contains("i")"#),
            Ok(Val::Some(Box::new(bool(true))))
        );
    }
}
