use std::fmt;

use chrono::TimeDelta;
use grove_query::typecheck::types::TypedMethodArg;
use grove_schema::validated::Config;
use grove_types::Span;
use rust_decimal::Decimal;

use super::super::arithmetic::{ArithOp, dec_arith, float_arith, int_arith, scale_duration};
use super::super::data::{owning_refs_in, prefetch_owning, resolve_base_link};
use super::super::{EvalCtx, eval};
use super::{pop_element_scope, push_element_scope};
use crate::error::ExecError;
use crate::value::{Scalar, Val};

#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum ReduceKind {
    Sum,
    Avg,
    Max,
    Min,
}

impl ReduceKind {
    pub(super) fn from_str(name: &str) -> Option<ReduceKind> {
        Some(match name {
            "sum" => ReduceKind::Sum,
            "avg" => ReduceKind::Avg,
            "max" => ReduceKind::Max,
            "min" => ReduceKind::Min,
            _ => return None,
        })
    }
}

impl fmt::Display for ReduceKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReduceKind::Sum => write!(f, "sum"),
            ReduceKind::Avg => write!(f, "avg"),
            ReduceKind::Max => write!(f, "max"),
            ReduceKind::Min => write!(f, "min"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum KeyedReduceKind {
    SumOver,
    AvgOver,
    MaxBy,
    MinBy,
}

impl KeyedReduceKind {
    pub(super) fn from_str(name: &str) -> Option<KeyedReduceKind> {
        Some(match name {
            "sum_over" => KeyedReduceKind::SumOver,
            "avg_over" => KeyedReduceKind::AvgOver,
            "max_by" => KeyedReduceKind::MaxBy,
            "min_by" => KeyedReduceKind::MinBy,
            _ => return None,
        })
    }
}

impl fmt::Display for KeyedReduceKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KeyedReduceKind::SumOver => write!(f, "sum_over"),
            KeyedReduceKind::AvgOver => write!(f, "avg_over"),
            KeyedReduceKind::MaxBy => write!(f, "max_by"),
            KeyedReduceKind::MinBy => write!(f, "min_by"),
        }
    }
}

/// Fold a list to one value
pub(super) fn eval_reduce(
    kind: ReduceKind,
    base: Val,
    optional: bool,
    ctx: &mut EvalCtx<'_>,
    span: Span,
) -> Result<Val, ExecError> {
    let items = match (optional, base) {
        (true, Val::None) => return Ok(Val::None),
        (true, Val::Some(v)) => match *v {
            Val::List(items) => items,
            other => {
                return Err(ExecError::Integrity {
                    message: format!(
                        "`?.{kind}()` called on non-list value: {other} (internal error)"
                    ),
                });
            }
        },
        (_, Val::List(items)) => items,
        (_, other) => {
            return Err(ExecError::Integrity {
                message: format!("`{kind}()` called on non-list value: {other} (internal error)"),
            });
        }
    };
    let config = &ctx.schema.config;
    let reduced = if items.is_empty() {
        Val::None
    } else {
        match kind {
            ReduceKind::Sum => Val::Scalar(sum_scalars(
                &collect_reduction_scalars(items, kind)?,
                config,
                span,
            )?),
            ReduceKind::Avg => Val::Scalar(avg_scalars(
                &collect_reduction_scalars(items, kind)?,
                config,
                span,
            )?),
            ReduceKind::Max => extremum(items, std::cmp::Ordering::Greater, span)?,
            ReduceKind::Min => extremum(items, std::cmp::Ordering::Less, span)?,
        }
    };
    let result = match reduced {
        Val::None => Val::None,
        value => Val::Some(Box::new(value)),
    };
    Ok(if optional {
        Val::Some(Box::new(result))
    } else {
        result
    })
}

/// Fold a list to one value by a per-element key
pub(super) fn eval_reduce_over(
    kind: KeyedReduceKind,
    base: Val,
    optional: bool,
    args: &[TypedMethodArg],
    ctx: &mut EvalCtx<'_>,
    span: Span,
) -> Result<Val, ExecError> {
    let mut items = match (optional, base) {
        (true, Val::None) => return Ok(Val::None),
        (true, Val::Some(v)) => match *v {
            Val::List(items) => items,
            other => {
                return Err(ExecError::Integrity {
                    message: format!(
                        "`?.{kind}()` called on non-list value: {other} (internal error)"
                    ),
                });
            }
        },
        (_, Val::List(items)) => items,
        (_, other) => {
            return Err(ExecError::Integrity {
                message: format!("`{kind}()` called on non-list value: {other} (internal error)"),
            });
        }
    };
    let [arg] = args else {
        return Err(ExecError::Integrity {
            message: format!("`{kind}()` expects one key argument (internal error)"),
        });
    };
    let fields = owning_refs_in(&arg.expr, ctx.schema, ctx.plan.prefetch_skip());
    prefetch_owning(ctx, &mut items, &fields)?;
    let config = &ctx.schema.config;
    let reduced = if items.is_empty() {
        Val::None
    } else {
        match kind {
            KeyedReduceKind::SumOver | KeyedReduceKind::AvgOver => {
                let mut keys = Vec::with_capacity(items.len());
                for item in &items {
                    let map_element = push_element_scope(ctx, item);
                    let key = eval(&arg.expr, ctx).and_then(|key| resolve_base_link(ctx, key));
                    pop_element_scope(ctx, map_element);
                    match key? {
                        Val::Scalar(scalar) => keys.push(scalar),
                        other => {
                            return Err(ExecError::Integrity {
                                message: format!(
                                    "`{kind}()` key evaluated to non-scalar: {other} (internal error)"
                                ),
                            });
                        }
                    }
                }
                let reduced = match kind {
                    KeyedReduceKind::SumOver => sum_scalars(&keys, config, span)?,
                    _ => avg_scalars(&keys, config, span)?,
                };
                Val::Some(Box::new(Val::Scalar(reduced)))
            }
            KeyedReduceKind::MaxBy | KeyedReduceKind::MinBy => {
                let want = if kind == KeyedReduceKind::MaxBy {
                    std::cmp::Ordering::Greater
                } else {
                    std::cmp::Ordering::Less
                };
                let mut best: Option<(Val, Val)> = None;
                for item in items {
                    let map_element = push_element_scope(ctx, &item);
                    let key = eval(&arg.expr, ctx).and_then(|key| resolve_base_link(ctx, key));
                    pop_element_scope(ctx, map_element);
                    let key = key?;
                    if key.contains_ref() {
                        continue;
                    }
                    let replace = match &best {
                        None => true,
                        Some((_, best_key)) => key.cmp(best_key, span)? == want,
                    };
                    if replace {
                        best = Some((item, key));
                    }
                }
                best.map_or(Val::None, |(item, _)| Val::Some(Box::new(item)))
            }
        }
    };
    Ok(if optional {
        Val::Some(Box::new(reduced))
    } else {
        reduced
    })
}

fn collect_reduction_scalars(items: Vec<Val>, kind: ReduceKind) -> Result<Vec<Scalar>, ExecError> {
    items
        .into_iter()
        .map(|item| match item {
            Val::Scalar(scalar) => Ok(scalar),
            other => Err(ExecError::Integrity {
                message: format!("`{kind}()` over non-scalar element: {other} (internal error)"),
            }),
        })
        .collect()
}

fn extremum(mut items: Vec<Val>, want: std::cmp::Ordering, span: Span) -> Result<Val, ExecError> {
    if items.is_empty() {
        return Err(ExecError::Integrity {
            message: "extremum over no values (internal error)".to_string(),
        });
    }
    let mut best_index = 0;
    for (index, item) in items[1..].iter().enumerate() {
        if item.cmp(&items[best_index], span)? == want {
            best_index = index + 1;
        }
    }
    Ok(items.swap_remove(best_index))
}

/// Sum non-empty same-typed scalars with config arithmetic
fn sum_scalars(values: &[Scalar], config: &Config, span: Span) -> Result<Scalar, ExecError> {
    let mut total = values
        .first()
        .cloned()
        .ok_or_else(|| ExecError::Integrity {
            message: "`sum()` over no values (internal error)".to_string(),
        })?;
    for value in &values[1..] {
        total = match (total, value) {
            (Scalar::Int(a), Scalar::Int(b)) => {
                int_arith(a, ArithOp::Add, *b, config.int_arithmetic, span)?
            }
            (Scalar::Float(a), Scalar::Float(b)) => {
                float_arith(a, ArithOp::Add, *b, config.float_checks, span)?
            }
            (Scalar::Dec(a), Scalar::Dec(b)) => {
                dec_arith(a, ArithOp::Add, *b, config.dec_arithmetic, span)?
            }
            (Scalar::Duration(a), Scalar::Duration(b)) => {
                Scalar::Duration(a.checked_add(b).ok_or_else(|| ExecError::Abort {
                    message: format!("duration overflow: {a} + {b}"),
                    span,
                })?)
            }
            _ => {
                return Err(ExecError::Integrity {
                    message: "`sum()` over mismatched values (internal error)".to_string(),
                });
            }
        };
    }
    Ok(total)
}

/// Average non-empty same-typed scalars with config arithmetic
fn avg_scalars(values: &[Scalar], config: &Config, span: Span) -> Result<Scalar, ExecError> {
    let first = values.first().ok_or_else(|| ExecError::Integrity {
        message: "`avg()` over no values (internal error)".to_string(),
    })?;
    let n = values.len();
    match first {
        Scalar::Instant(_) => return avg_instants(values, span),
        Scalar::Int(first) => {
            let mut total = *first as i128;
            for value in &values[1..] {
                let Scalar::Int(summand) = value else {
                    return Err(ExecError::Integrity {
                        message: "`avg()` over mismatched values (internal error)".to_string(),
                    });
                };
                total += *summand as i128;
            }
            let sum = total / n as i128;
            return Ok(Scalar::Int(sum as i64));
        }
        _ => {}
    }
    let sum = sum_scalars(values, config, span)?;
    match sum {
        Scalar::Float(total) => {
            float_arith(total, ArithOp::Div, n as f64, config.float_checks, span)
        }
        Scalar::Dec(total) => dec_arith(
            total,
            ArithOp::Div,
            Decimal::from(n as u64),
            config.dec_arithmetic,
            span,
        ),
        Scalar::Duration(total) => scale_duration(total, Decimal::from(n as u64), true, span),
        _ => Err(ExecError::Integrity {
            message: "`avg()` over mismatched values (internal error)".to_string(),
        }),
    }
}

/// Average instants by computing average offset from first
fn avg_instants(values: &[Scalar], span: Span) -> Result<Scalar, ExecError> {
    let Some(Scalar::Instant(start)) = values.first() else {
        return Err(ExecError::Integrity {
            message: "`avg()` over no values (internal error)".to_string(),
        });
    };
    let mut total_offset = TimeDelta::zero();
    for value in &values[1..] {
        let Scalar::Instant(instant) = value else {
            return Err(ExecError::Integrity {
                message: "`avg()` over mismatched values (internal error)".to_string(),
            });
        };
        total_offset = total_offset
            .checked_add(&instant.signed_duration_since(*start))
            .ok_or_else(|| ExecError::Abort {
                message: format!("duration overflow averaging instants around {start}"),
                span,
            })?;
    }
    let Scalar::Duration(mean_offset) =
        scale_duration(total_offset, Decimal::from(values.len() as u64), true, span)?
    else {
        return Err(ExecError::Integrity {
            message: "`avg()` offset duration was not a duration (internal error)".to_string(),
        });
    };
    start
        .checked_add_signed(mean_offset)
        .map(Scalar::Instant)
        .ok_or_else(|| ExecError::Abort {
            message: format!("instant overflow: {start} + {mean_offset}"),
            span,
        })
}

#[cfg(test)]
mod tests {
    use crate::error::ExecError;
    use crate::eval::harness::*;
    use crate::value::{Scalar, Val};
    use chrono::{DateTime, TimeDelta};
    use rust_decimal::dec;

    #[test]
    fn method_reduce_keyless() {
        for (src, expected) in [
            ("[1, 2, 3].sum()", some(int(6))),
            ("[1.5f, 2.5f].sum()", some(float(4.0))),
            (
                "[30.00, 0.1].sum()",
                some(Val::Scalar(Scalar::Dec(dec!(30.1)))),
            ),
            (
                "[#90s, #1.5s].sum()",
                some(Val::Scalar(Scalar::Duration(TimeDelta::milliseconds(
                    91500,
                )))),
            ),
            ("[1, 2, 4].avg()", some(int(2))),
            ("[1.0, 2.0].avg()", some(dec(dec!(1.5)))),
            ("[1.5f, 2.5f].avg()", some(float(2.0))),
            (
                "[30.00, 0.10].avg()",
                some(Val::Scalar(Scalar::Dec(dec!(15.05)))),
            ),
            (
                "[#90s, #1.5s].avg()",
                some(Val::Scalar(Scalar::Duration(TimeDelta::milliseconds(
                    45750,
                )))),
            ),
            (
                "[@1970-01-01, @1970-01-03_02:00].avg()",
                some(Val::Scalar(Scalar::Instant(
                    DateTime::from_timestamp(25 * 60 * 60, 0).unwrap(),
                ))),
            ),
            ("[3, 1, 2].max()", some(int(3))),
            ("[3, 1, 2].min()", some(int(1))),
            ("[\"b\", \"a\"].max()", some(str("b"))),
            (
                "[(1, \"b\"), (1, \"a\")].max()",
                some(Val::Tuple(vec![int(1), str("b")])),
            ),
            (
                "[(\"b\", 1), (\"a\", 2)].min()",
                some(Val::Tuple(vec![str("a"), int(2)])),
            ),
        ] {
            assert_eq!(run_src(src), Ok(expected), "{src}");
        }
        assert_eq!(
            run_seeded("users.first()?.orders?.amount?.sum()"),
            Ok(some(some(int(300))))
        );
        for (src, expected) in [
            ("users.id.sum()", some(int(15))),
            ("users.id.max()", some(int(5))),
            ("users.name.min()", some(str("Ada"))),
            (
                "users.balance.sum()",
                some(Val::Scalar(Scalar::Dec(dec!(1000024.85)))),
            ),
            (
                "users.balance.avg()",
                some(Val::Scalar(Scalar::Dec(dec!(200004.97)))),
            ),
        ] {
            assert_eq!(run_seeded(src), Ok(expected), "{src}");
        }
        for src in [
            "[1].skip(1).sum()",
            "[4.0].skip(1).avg()",
            "[true].skip(1).max()",
            "[\"d\"].skip(1).min()",
        ] {
            assert_eq!(run_src(src), Ok(Val::None), "{src}");
        }
        assert_eq!(
            run_seeded("users.elapsed.avg()?.as_seconds()"),
            Ok(some(Val::Scalar(Scalar::Dec(dec!(18.024691358)))))
        );
        match run_src("[Int::MAX, 1].sum()") {
            Err(ExecError::Abort { message, .. }) => {
                assert!(message.contains("integer overflow"), "got: {message}")
            }
            got => panic!("expected abort, got {got:?}"),
        }
        match run_src("[Float::MAX, Float::MAX].sum()") {
            Err(ExecError::Abort { message, .. }) => {
                assert!(
                    message.contains("non-finite float result"),
                    "got: {message}"
                )
            }
            got => panic!("expected abort, got {got:?}"),
        }
    }

    #[test]
    fn method_reduce_keyed() {
        for (src, expected) in [
            ("[-3, 2, 3].sum_over($ + 1)", some(int(5))),
            ("[10].sum_over($ - 2)", some(int(8))),
            ("[1, 2, 3].avg_over($ * $)", some(int(4))),
            ("[1.5f, 2.5f].avg_over($ + 0.5f)", some(float(2.5))),
            (
                "[30.000, 0.10].avg_over($ / 3.0)",
                some(Val::Scalar(Scalar::Dec(dec!(
                    5.0166666666666666666666666665
                )))),
            ),
        ] {
            assert_eq!(run_src(src), Ok(expected), "{src}");
        }
        assert_eq!(run_src("[3, 1, 2].min_by($ + 10)"), Ok(some(int(1))));
        assert_eq!(
            run_src("[(1, \"a\"), (1, \"b\")].max_by($.0)"),
            Ok(some(Val::Tuple(vec![int(1), str("a")]))),
        );
        assert_eq!(
            run_src("[(2, \"b\"), (1, \"a\")].min_by($.1)"),
            Ok(some(Val::Tuple(vec![int(1), str("a")]))),
        );
        for src in [
            "[1].skip(1).sum_over($ / 0)",
            "[1].skip(1).avg_over($ / 0)",
            "[1].skip(1).max_by($ / 0)",
            "[1].skip(1).min_by($ / 0)",
        ] {
            assert_eq!(run_src(src), Ok(Val::None), "{src}");
        }

        assert_eq!(
            run_seeded("users { t = orders.sum_over(amount) }.t"),
            Ok(Val::List(vec![
                some(int(300)),
                some(int(400)),
                Val::None,
                Val::None,
                Val::None
            ])),
        );
        assert_eq!(
            run_seeded("users.orders.map($.avg_over(amount))"),
            Ok(Val::List(vec![
                some(int(150)),
                some(int(400)),
                Val::None,
                Val::None,
                Val::None
            ])),
        );
        assert_eq!(
            run_seeded("users { t = orders.sum_over(amount + id) }.t"),
            Ok(Val::List(vec![
                some(int(302)),
                some(int(402)),
                Val::None,
                Val::None,
                Val::None
            ])),
        );
        assert_eq!(
            run_seeded("users { m = orders.max_by(amount)?.amount }.m"),
            Ok(Val::List(vec![
                some(int(200)),
                some(int(400)),
                Val::None,
                Val::None,
                Val::None
            ])),
        );
    }

    #[test]
    fn method_reduce_optional() {
        assert_eq!(
            run_seeded("users.first()?.lucky?.map($.unwrap_or(0))?.sum()"),
            Ok(some(some(int(5))))
        );
        assert_eq!(
            run_seeded("users.first()?.orders?.max_by(amount).unwrap()?.amount"),
            Ok(some(int(200)))
        );
        assert_eq!(
            run_seeded("users.skip(5).first()?.lucky?.map($.unwrap_or(0))?.sum()"),
            Ok(Val::None)
        );
        assert_eq!(
            run_seeded("users.skip(5).first()?.orders?.max_by(amount)"),
            Ok(Val::None)
        );
        assert_eq!(
            run_seeded("users.first()?.orders?.map($.amount)?.sum()"),
            Ok(some(some(int(300))))
        );
        assert_eq!(
            run_seeded("users.skip(2).first()?.orders?.sum_over(amount)"),
            Ok(some(Val::None))
        );
    }
}
