use std::cell::RefCell;
use std::collections::HashSet;
use std::fmt;

use grove_query::ast::SortDir;
use grove_query::typecheck::types::TypedMethodArg;
use grove_types::Span;

use super::super::data::{prefetch_owning, resolve_base_link};
use super::super::{EvalCtx, eval};
use super::{pop_element_scope, push_element_scope};
use crate::error::ExecError;
use crate::eval::data::collect_owning_refs_in;
use crate::value::Val;

#[derive(Debug, Clone, Copy)]
pub(super) enum SortKind {
    Asc,
    Desc,
    AscKeyed,
    DescKeyed,
}

impl SortKind {
    pub(super) fn from_str(name: &str) -> Option<SortKind> {
        Some(match name {
            "sort" | "sort_asc" => SortKind::Asc,
            "sort_desc" => SortKind::Desc,
            "sort_over" | "sort_asc_over" => SortKind::AscKeyed,
            "sort_desc_over" => SortKind::DescKeyed,
            _ => return None,
        })
    }

    pub(super) fn is_keyed(&self) -> bool {
        matches!(self, SortKind::AscKeyed | SortKind::DescKeyed)
    }

    pub(super) fn is_desc(&self) -> bool {
        matches!(self, SortKind::Desc | SortKind::DescKeyed)
    }

    pub(super) fn sort_dir(&self) -> SortDir {
        match self {
            SortKind::Asc | SortKind::AscKeyed => SortDir::Asc,
            SortKind::Desc | SortKind::DescKeyed => SortDir::Desc,
        }
    }
}

impl fmt::Display for SortKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SortKind::Asc => write!(f, "sort_asc"),
            SortKind::Desc => write!(f, "sort_desc"),
            SortKind::AscKeyed => write!(f, "sort_asc_over"),
            SortKind::DescKeyed => write!(f, "sort_desc_over"),
        }
    }
}

/// Sort a List by value or key expressions (stable)
pub(super) fn eval_sort(
    kind: SortKind,
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
    if kind.is_keyed() {
        let mut fields = HashSet::new();
        for arg in args {
            collect_owning_refs_in(&arg.expr, ctx.schema, &mut fields);
        }
        prefetch_owning(ctx, &mut items, &fields)?;
        sort_by_keys(&mut items, kind, args, ctx, span)?;
    } else if !args.is_empty() {
        return Err(ExecError::Integrity {
            message: format!("`{kind}()` takes no key arguments (internal error)"),
        });
    } else {
        sort_keyless(&mut items, kind, span)?;
    }
    let sorted = Val::List(items);
    Ok(if optional {
        Val::Some(Box::new(sorted))
    } else {
        sorted
    })
}

/// Keyless sort (stable) over values
fn sort_keyless(items: &mut [Val], kind: SortKind, span: Span) -> Result<(), ExecError> {
    let mut result = Ok(());
    items.sort_by(|a, b| match (result.as_ref(), a.cmp(b, span)) {
        (Ok(_), Ok(order)) => {
            if kind.is_desc() {
                order.reverse()
            } else {
                order
            }
        }
        (Ok(_), Err(e)) => {
            result = Err(e);
            std::cmp::Ordering::Equal
        }
        (Err(_), _) => std::cmp::Ordering::Equal,
    });
    result
}

// TODO: batch eval first key
/// Keyed sort (stable) with per-key expressions, directions
fn sort_by_keys(
    items: &mut Vec<Val>,
    kind: SortKind,
    args: &[TypedMethodArg],
    ctx: &mut EvalCtx<'_>,
    span: Span,
) -> Result<(), ExecError> {
    let mut failure: Option<ExecError> = None;
    let mut items_key_cache: Vec<(Val, RefCell<Vec<Val>>)> = std::mem::take(items)
        .into_iter()
        .map(|item| (item, RefCell::new(Vec::new())))
        .collect();
    items_key_cache.sort_by(|(a, cache_a), (b, cache_b)| {
        if failure.is_some() {
            return std::cmp::Ordering::Equal;
        }
        match compare_keys_eval_cached(a, b, cache_a, cache_b, args, kind.sort_dir(), ctx, span) {
            Ok(order) => order,
            Err(e) => {
                failure = Some(e);
                std::cmp::Ordering::Equal
            }
        }
    });
    if let Some(e) = failure {
        return Err(e);
    }
    *items = items_key_cache.into_iter().map(|(item, _)| item).collect();
    Ok(())
}

/// Compare sort keys, with on-demand evaluation and caching
#[allow(clippy::too_many_arguments)]
fn compare_keys_eval_cached(
    a: &Val,
    b: &Val,
    cache_a: &RefCell<Vec<Val>>,
    cache_b: &RefCell<Vec<Val>>,
    keys: &[TypedMethodArg],
    default_dir: SortDir,
    ctx: &mut EvalCtx<'_>,
    span: Span,
) -> Result<std::cmp::Ordering, ExecError> {
    for (i, arg) in keys.iter().enumerate() {
        if cache_a.borrow().len() <= i {
            let key = eval_sort_key(ctx, arg, a)?;
            cache_a.borrow_mut().push(key);
        }
        if cache_b.borrow().len() <= i {
            let key = eval_sort_key(ctx, arg, b)?;
            cache_b.borrow_mut().push(key);
        }
        let key_a = &cache_a.borrow()[i];
        let key_b = &cache_b.borrow()[i];
        // Refs are considered undecidable for now
        if key_a.contains_ref() || key_b.contains_ref() {
            continue;
        }
        match key_a.cmp(key_b, span)? {
            std::cmp::Ordering::Equal => continue,
            non_equal => {
                return Ok(
                    if arg.direction.as_deref().copied().unwrap_or(default_dir) == SortDir::Desc {
                        non_equal.reverse()
                    } else {
                        non_equal
                    },
                );
            }
        }
    }
    Ok(std::cmp::Ordering::Equal)
}

// TODO: resolve owning Refs
/// Evaluate one sort key expr against an element scope
fn eval_sort_key(
    ctx: &mut EvalCtx<'_>,
    arg: &TypedMethodArg,
    item: &Val,
) -> Result<Val, ExecError> {
    let map_element = push_element_scope(ctx, item);
    let key = eval(&arg.expr, ctx).map(|key| resolve_base_link(ctx, key));
    pop_element_scope(ctx, map_element);
    key?
}

#[cfg(test)]
mod tests {
    use crate::eval::harness::*;
    use crate::value::{Scalar, Val};

    #[test]
    fn method_sort_scalars() {
        let run_sort = |src: &str| -> Vec<Val> {
            match run_src(src) {
                Ok(Val::List(elements)) => elements,
                got => panic!("expected List, got: {got:?}"),
            }
        };
        assert_eq!(
            run_sort("[4,2,5,3,1].sort()"),
            vec![int(1), int(2), int(3), int(4), int(5)]
        );
        assert_eq!(
            run_sort(r#"["def", "abc", "xyz", "ab"].sort_desc()"#),
            vec![str("xyz"), str("def"), str("abc"), str("ab")]
        );
        assert_eq!(run_sort("[0.0f].sort_asc()"), vec![float(0.0)]);
        assert_eq!(
            run_sort("[1, 4, 2, 5, 3].sort_over(-$)"),
            [int(5), int(4), int(3), int(2), int(1)],
        );
        assert_eq!(
            run_sort("[2, 3, 1].sort_over(-$ + [4, 5].sort_desc_over($$ * -2 + $).len())"),
            [int(3), int(2), int(1)],
        );
    }

    #[test]
    fn method_sort_stable_composition() {
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
        // rowid order within grounps
        assert_eq!(
            names("users.sort_over(active)"),
            [str("Cyd"), str("Eli"), str("Ada"), str("Bob"), str("Dan")]
        );
        assert_eq!(
            names("users.skip(2).sort_desc_over(name)"),
            [str("Eli"), str("Dan"), str("Cyd")]
        );
        match run_seeded("users { o = orders.sort_desc_over(amount) }") {
            Ok(Val::List(rows)) => {
                let Val::Record(first) = &rows[0] else {
                    panic!("expected record");
                };
                match first.iter().find(|(name, _)| name.as_ref() == "o") {
                    Some((_, Val::List(orders))) => {
                        assert_eq!(orders.len(), 2);
                        let amounts: Vec<i64> = orders
                            .iter()
                            .map(|order| match order {
                                Val::Record(fields) => {
                                    match fields.iter().find(|(name, _)| name.as_ref() == "amount")
                                    {
                                        Some((_, Val::Scalar(Scalar::Int(n)))) => *n,
                                        got => panic!("expected amount, got {got:?}"),
                                    }
                                }
                                other => panic!("expected order record, got {other}"),
                            })
                            .collect();
                        assert_eq!(amounts, [200, 100]);
                    }
                    got => panic!("expected orders list, got {got:?}"),
                }
            }
            got => panic!("expected projected users, got {got:?}"),
        }
    }

    #[test]
    fn method_sort_keyless_composition() {
        match run_seeded("users { t = tags.sort_desc() }") {
            Ok(Val::List(rows)) => {
                let Val::Record(second) = &rows[1] else {
                    panic!("expected record");
                };
                match second.iter().find(|(name, _)| name.as_ref() == "t") {
                    Some((_, Val::List(tags))) => {
                        assert_eq!(tags, &vec![str("b2"), str("b1")]);
                    }
                    got => panic!("expected tags list, got {got:?}"),
                }
            }
            got => panic!("expected projected users, got {got:?}"),
        }
    }

    #[test]
    fn method_sort_over() {
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
            names("users.sort_over(name)"),
            [str("Ada"), str("Bob"), str("Cyd"), str("Dan"), str("Eli")],
        );
        assert_eq!(
            names("users.sort_desc_over(name)"),
            [str("Eli"), str("Dan"), str("Cyd"), str("Bob"), str("Ada")],
        );
        assert_eq!(
            names("users.sort_desc_over(asc name)"),
            names("users.sort_over(name)"),
        );
        assert_eq!(
            names("users.sort_over(active, name)"),
            [str("Cyd"), str("Eli"), str("Ada"), str("Bob"), str("Dan")],
        );
        assert_eq!(
            names("users.sort_over(desc active, asc name)"),
            [str("Ada"), str("Bob"), str("Dan"), str("Cyd"), str("Eli")],
        );
    }
}
