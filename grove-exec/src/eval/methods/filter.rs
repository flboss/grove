use grove_query::typecheck::types::TypedMethodArg;

use super::super::data::resolve_base_link;
use super::super::{EvalCtx, eval};
use super::{pop_element_scope, push_element_scope};
use crate::error::ExecError;
use crate::value::{Scalar, Val};

pub(super) fn eval_filter(
    base: Val,
    optional: bool,
    args: &[TypedMethodArg],
    ctx: &mut EvalCtx<'_>,
) -> Result<Val, ExecError> {
    let items = match (optional, base) {
        (true, Val::None) => return Ok(Val::None),
        (true, Val::Some(v)) => match *v {
            Val::List(items) => items,
            other => {
                return Err(ExecError::Integrity {
                    message: format!(
                        "`?.filter()` called on non-list value: {other} (internal error)"
                    ),
                });
            }
        },
        (_, Val::List(items)) => items,
        (_, other) => {
            return Err(ExecError::Integrity {
                message: format!("`filter()` called on non-list value: {other} (internal error)"),
            });
        }
    };
    let [arg] = args else {
        return Err(ExecError::Integrity {
            message: "`filter()` expects one argument (internal error)".to_string(),
        });
    };
    let mut kept = Vec::with_capacity(items.len());
    for item in items {
        let map_element = push_element_scope(ctx, &item);
        let pred = eval(&arg.expr, ctx).and_then(|pred| resolve_base_link(ctx, pred));
        pop_element_scope(ctx, map_element);
        match pred? {
            Val::Scalar(Scalar::Bool(true)) => kept.push(item),
            Val::Scalar(Scalar::Bool(false)) => {}
            other => {
                return Err(ExecError::Integrity {
                    message: format!(
                        "`filter()` predicate evaluated to non-Bool: {other} (internal error)"
                    ),
                });
            }
        }
    }
    let filtered = Val::List(kept);
    Ok(if optional {
        Val::Some(Box::new(filtered))
    } else {
        filtered
    })
}

#[cfg(test)]
mod tests {
    use crate::error::ExecError;
    use crate::eval::harness::*;
    use crate::value::Val;

    #[test]
    fn method_filter_records() {
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
        assert_eq!(names("users[active]"), [str("Ada"), str("Bob"), str("Dan")]);
        assert_eq!(names("users.filter(active)"), names("users[active]"));
        assert_eq!(names("users[id > 2]"), [str("Cyd"), str("Dan"), str("Eli")]);
        assert_eq!(
            names("users[name.contains(\"A\".to_lower())]"),
            [str("Ada"), str("Dan")]
        );
        assert_eq!(names("users[active && id > 1]"), [str("Bob"), str("Dan")]);
        assert_eq!(names("users[nickname == some(\"A\")]"), [str("Ada")]);
        assert_eq!(
            names("users[profile?.bio?.len() == some(6)]"),
            [str("Ada"), str("Cyd")]
        );
        assert_eq!(
            names("users[manager?.name == users[name.starts_with(\"B\")].first()?.name]"),
            [str("Ada"), str("Cyd")]
        );
        assert_eq!(names("users[orders.len() > 1]"), [str("Ada")]);
        assert_eq!(
            names("users[orders.len() == 0]"),
            [str("Cyd"), str("Dan"), str("Eli")]
        );
        assert_eq!(
            names("users[orders[amount > 150].len() == 1]"),
            [str("Ada"), str("Bob")]
        );
    }

    #[test]
    fn method_filter_scalars() {
        let run_filter = |src: &str| match run_src(src) {
            Ok(Val::List(elements)) => elements,
            got => panic!("expected List, got: {got:?}"),
        };
        assert_eq!(
            run_filter("[4, 2, 5, 3, 1].filter($ > 2)"),
            vec![int(4), int(5), int(3)]
        );
        assert_eq!(run_filter("[-1, -2, 2, 3][$ - 2 > 0]"), vec![int(3)]);
        assert_eq!(
            run_filter("[\"a\", \"bb\", \"c\"][!($.len() <= 1)]"),
            vec![str("bb")]
        );
        assert_eq!(
            run_filter("[[1, 2], [5]][$[$$ > 2].len() > 0]"),
            vec![Val::List(vec![int(5)])]
        );
        assert_eq!(run_filter("[1, 2, 3].filter($ > 10)"), Vec::new());
        assert_eq!(run_filter("[1][$ > 10][$ > 100]"), Vec::new());
        assert_eq!(
            match run_seeded("users.tags.flatten().filter($ == \"a1\")") {
                Ok(Val::List(elements)) => elements,
                got => panic!("expected List, got: {got:?}"),
            },
            vec![str("a1"), str("a1")]
        );
    }

    #[test]
    fn method_filter_optional() {
        let amounts = |src: &str| match run_seeded(src) {
            Ok(Val::Some(list)) => match *list {
                Val::List(orders) => orders
                    .into_iter()
                    .map(|order| match order {
                        Val::Record(fields) => fields
                            .into_iter()
                            .find_map(|(name, value)| (name.as_ref() == "amount").then_some(value))
                            .expect("expected `amount` field"),
                        other => panic!("expected order record, got {other}"),
                    })
                    .collect::<Vec<_>>(),
                got => panic!("expected orders list, got {got:?}"),
            },
            got => panic!("expected some(orders) for {src}, got {got:?}"),
        };
        assert_eq!(
            amounts("users.first()?.orders?.filter(amount > 150)"),
            [int(200)]
        );
        assert_eq!(
            run_seeded("users.skip(5).first()?.orders?.filter(amount > 0)"),
            Ok(Val::None)
        );
    }

    #[test]
    fn method_filter_composition() {
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
        assert_eq!(names("users[active][id > 1]"), [str("Bob"), str("Dan")]);
        assert_eq!(names("users[active][!active]"), []);
        assert_eq!(
            names("users.sort_desc_over(id).take(2)[active]"),
            [str("Dan")]
        );
        assert_eq!(names("users.skip(5)[active]"), Vec::new());
        assert_eq!(names("users[active][id > 100]"), Vec::new());
        match run_seeded("users[id == 1 / 0]") {
            Err(ExecError::Abort { message, .. }) => {
                assert!(message.contains("division by zero"), "got: {message}")
            }
            got => panic!("expected abort, got {got:?}"),
        }
    }

    #[test]
    fn method_filter_projection() {
        match run_seeded("users { o = orders[amount > 150] }") {
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
                let Val::Record(first) = &rows[0] else {
                    panic!("expected record");
                };
                match first.iter().find(|(name, _)| name.as_ref() == "o") {
                    Some((_, Val::List(orders))) => {
                        let Val::Record(order) = &orders[0] else {
                            panic!("expected order record");
                        };
                        assert!(order.iter().any(|(name, value)| {
                            name.as_ref() == "amount" && *value == int(200)
                        }));
                    }
                    got => panic!("expected orders list, got {got:?}"),
                }
            }
            got => panic!("expected projected users, got {got:?}"),
        }
        match run_seeded("roles { u = users[active] { name, o = orders[amount > 100] } }") {
            Ok(Val::List(rows)) => {
                assert_eq!(rows.len(), 2);
                let user_names = |row: &Val| match row {
                    Val::Record(fields) => {
                        match fields.iter().find(|(name, _)| name.as_ref() == "u") {
                            Some((_, Val::List(users))) => users
                                .iter()
                                .map(|user| match user {
                                    Val::Record(user) => user
                                        .iter()
                                        .find_map(|(name, value)| {
                                            (name.as_ref() == "name").then_some(value.clone())
                                        })
                                        .expect("expected `name` field"),
                                    other => panic!("expected user record, got {other}"),
                                })
                                .collect::<Vec<_>>(),
                            got => panic!("expected users list, got {got:?}"),
                        }
                    }
                    other => panic!("expected record, got {other}"),
                };
                assert_eq!(user_names(&rows[0]), [str("Ada")]);
                assert_eq!(user_names(&rows[1]), [str("Ada"), str("Ada")]);
                let Val::Record(admin) = &rows[0] else {
                    panic!("expected record");
                };
                match admin.iter().find(|(name, _)| name.as_ref() == "u") {
                    Some((_, Val::List(users))) => {
                        let Val::Record(user) = &users[0] else {
                            panic!("expected user record");
                        };
                        match user.iter().find(|(name, _)| name.as_ref() == "o") {
                            Some((_, Val::List(orders))) => {
                                assert_eq!(orders.len(), 1);
                                let Val::Record(order) = &orders[0] else {
                                    panic!("expected order record");
                                };
                                assert!(order.iter().any(|(name, value)| {
                                    name.as_ref() == "amount" && *value == int(200)
                                }));
                            }
                            got => panic!("expected orders list, got {got:?}"),
                        }
                    }
                    got => panic!("expected users list, got {got:?}"),
                }
            }
            got => panic!("expected projected roles, got {got:?}"),
        }
    }
}
