
#[allow(dead_code)]
#[path = "common.rs"]
mod common;

use common::{compile, dec, fixture};
use grove_query::codegen::model::{
    Cardinality, ChildRef, CompiledQuery, ParentRef, PlannedStatement, ResultShape, RowCount,
    StatementKind, StatementShape,
};
use grove_runtime::exec::{Row, StatementOutput};
use grove_runtime::{RuntimeError, Value, assemble};
use std::collections::HashMap;

fn value(query: &str) -> Value {
    let mut rt = fixture();
    let compiled = compile(query);
    rt.execute_value(&compiled).unwrap()
}

fn row(pairs: &[(&str, Value)]) -> Row {
    Row {
        values: pairs
            .iter()
            .map(|(n, v)| (n.to_string(), v.clone()))
            .collect(),
    }
}

fn int_col(name: &str) -> ResultShape {
    ResultShape::Column(name.to_string())
}

fn unkeyed(stmt: usize, cardinality: Cardinality) -> ChildRef {
    ChildRef {
        stmt,
        parent: None,
        cardinality,
        tag: None,
    }
}

fn keyed(stmt: usize, parent: usize, parent_key: &str, child_key: &str) -> ChildRef {
    ChildRef {
        stmt,
        parent: Some(ParentRef {
            stmt: parent,
            parent_key: parent_key.to_string(),
            child_key: child_key.to_string(),
            negated: false,
        }),
        cardinality: Cardinality::Many,
        tag: None,
    }
}


#[test]
fn flat_record_tree() {
    assert_eq!(
        value("users[age > 30] { name, age }"),
        Value::List(vec![Value::Record(vec![
            ("name".to_string(), Value::String("Bob".to_string())),
            ("age".to_string(), Value::Int(40)),
        ])])
    );
}

#[test]
fn scalar_root() {
    assert_eq!(value("1 + 2"), Value::Int(3));
}

#[test]
fn tuple_root() {
    assert_eq!(
        value("(1, 2.5)"),
        Value::Tuple(vec![Value::Int(1), Value::Dec(dec("2.5"))])
    );
}

#[test]
fn nested_keyed_tree() {
    assert_eq!(
        value("users { name, o = orders { total } }"),
        Value::List(vec![
            Value::Record(vec![
                ("name".to_string(), Value::String("Ada".to_string())),
                (
                    "o".to_string(),
                    Value::List(vec![
                        Value::Record(vec![("total".to_string(), Value::Dec(dec("10")))]),
                        Value::Record(vec![("total".to_string(), Value::Dec(dec("20")))]),
                    ])
                ),
            ]),
            Value::Record(vec![
                ("name".to_string(), Value::String("Bob".to_string())),
                (
                    "o".to_string(),
                    Value::List(vec![Value::Record(vec![(
                        "total".to_string(),
                        Value::Dec(dec("30"))
                    )])]),
                ),
            ]),
        ])
    );
}

#[test]
fn broadcast_literal_field() {
    assert_eq!(
        value("users.take(1) { name, ls = [1, 2] }"),
        Value::List(vec![Value::Record(vec![
            ("name".to_string(), Value::String("Ada".to_string())),
            (
                "ls".to_string(),
                Value::List(vec![Value::Int(1), Value::Int(2)])
            ),
        ])])
    );
}

#[test]
fn grandchild_tree_pairs_transitively() {
    assert_eq!(
        value("users { name, o = orders { total, i = items { sku } } }"),
        Value::List(vec![
            Value::Record(vec![
                ("name".to_string(), Value::String("Ada".to_string())),
                (
                    "o".to_string(),
                    Value::List(vec![
                        Value::Record(vec![
                            ("total".to_string(), Value::Dec(dec("10"))),
                            (
                                "i".to_string(),
                                Value::List(vec![
                                    Value::Record(vec![(
                                        "sku".to_string(),
                                        Value::String("a".to_string())
                                    )]),
                                    Value::Record(vec![(
                                        "sku".to_string(),
                                        Value::String("b".to_string())
                                    )]),
                                ])
                            ),
                        ]),
                        Value::Record(vec![
                            ("total".to_string(), Value::Dec(dec("20"))),
                            ("i".to_string(), Value::List(vec![])),
                        ]),
                    ])
                ),
            ]),
            Value::Record(vec![
                ("name".to_string(), Value::String("Bob".to_string())),
                (
                    "o".to_string(),
                    Value::List(vec![Value::Record(vec![
                        ("total".to_string(), Value::Dec(dec("30"))),
                        (
                            "i".to_string(),
                            Value::List(vec![Value::Record(vec![(
                                "sku".to_string(),
                                Value::String("c".to_string())
                            )])])
                        ),
                    ])]),
                ),
            ]),
        ])
    );
}

#[test]
fn grandchild_single_pairs_per_middle_row() {
    assert_eq!(
        value("users { o = orders { c = items.first()?.sku } }"),
        Value::List(vec![
            Value::Record(vec![(
                "o".to_string(),
                Value::List(vec![
                    Value::Record(vec![("c".to_string(), Value::String("a".to_string()))]),
                    Value::Record(vec![("c".to_string(), Value::Null)]),
                ])
            )]),
            Value::Record(vec![(
                "o".to_string(),
                Value::List(vec![Value::Record(vec![(
                    "c".to_string(),
                    Value::String("c".to_string())
                )])])
            )]),
        ])
    );
}

#[test]
fn depth_three_single_pairs_transitively() {
    assert_eq!(
        value("users { o = orders { i = items { w = order } } }"),
        Value::List(vec![
            Value::Record(vec![(
                "o".to_string(),
                Value::List(vec![
                    Value::Record(vec![(
                        "i".to_string(),
                        Value::List(vec![
                            Value::Record(vec![(
                                "w".to_string(),
                                Value::Record(vec![("total".to_string(), Value::Dec(dec("10")))]),
                            )]),
                            Value::Record(vec![(
                                "w".to_string(),
                                Value::Record(vec![("total".to_string(), Value::Dec(dec("10")))]),
                            )]),
                        ])
                    )]),
                    Value::Record(vec![("i".to_string(), Value::List(vec![]),)]),
                ])
            )]),
            Value::Record(vec![(
                "o".to_string(),
                Value::List(vec![Value::Record(vec![(
                    "i".to_string(),
                    Value::List(vec![Value::Record(vec![(
                        "w".to_string(),
                        Value::Record(vec![("total".to_string(), Value::Dec(dec("30")))]),
                    )])])
                ),])]),
            )]),
        ])
    );
}

#[test]
fn empty_root_list() {
    assert_eq!(value("users[age > 100] { name }"), Value::List(vec![]));
}

#[test]
fn optional_list_chain_nones_vs_empties() {
    assert_eq!(
        value("users { name, o = orders.sort(total).first()?.items }"),
        Value::List(vec![
            Value::Record(vec![
                ("name".to_string(), Value::String("Ada".to_string())),
                (
                    "o".to_string(),
                    Value::List(vec![
                        Value::Record(vec![("sku".to_string(), Value::String("a".to_string()))]),
                        Value::Record(vec![("sku".to_string(), Value::String("b".to_string()))]),
                    ])
                ),
            ]),
            Value::Record(vec![
                ("name".to_string(), Value::String("Bob".to_string())),
                (
                    "o".to_string(),
                    Value::List(vec![Value::Record(vec![(
                        "sku".to_string(),
                        Value::String("c".to_string())
                    )])])
                ),
            ]),
        ])
    );
}

#[test]
fn optional_list_literal_present_vs_empty() {
    assert_eq!(
        value("users.take(1) { t = [[1]].first() }"),
        Value::List(vec![Value::Record(vec![(
            "t".to_string(),
            Value::List(vec![Value::Int(1)])
        )])])
    );
    assert_eq!(
        value("users.take(1) { e = [[], [1]].first() }"),
        Value::List(vec![Value::Record(vec![(
            "e".to_string(),
            Value::List(vec![])
        )])])
    );
}

#[test]
fn unwrap_row_asserts_value() {
    assert_eq!(
        value("users[age > 30].first().unwrap() { name }"),
        Value::List(vec![Value::Record(vec![(
            "name".to_string(),
            Value::String("Bob".to_string())
        )])])
    );
    assert_eq!(
        value("[[1]].first().unwrap()"),
        Value::List(vec![Value::List(vec![Value::Int(1)])])
    );
}

#[test]
fn unwrap_row_empty_aborts() {
    let mut rt = fixture();
    let compiled = compile("users[age > 100].first().unwrap() { name }");
    let err = rt.execute_value(&compiled).unwrap_err();
    assert!(
        matches!(err, RuntimeError::Aborted(_)),
        "expected abort, got {err:?}"
    );
}

#[test]
fn collapse_is_some_is_none_values() {
    assert_eq!(
        value("users { name, h = orders.first().is_some() }"),
        Value::List(vec![
            Value::Record(vec![
                ("name".to_string(), Value::String("Ada".to_string())),
                ("h".to_string(), Value::Bool(true)),
            ]),
            Value::Record(vec![
                ("name".to_string(), Value::String("Bob".to_string())),
                ("h".to_string(), Value::Bool(true)),
            ]),
        ])
    );
    assert_eq!(
        value("users { name, h = orders.first().is_none() }"),
        Value::List(vec![
            Value::Record(vec![
                ("name".to_string(), Value::String("Ada".to_string())),
                ("h".to_string(), Value::Bool(false)),
            ]),
            Value::Record(vec![
                ("name".to_string(), Value::String("Bob".to_string())),
                ("h".to_string(), Value::Bool(false)),
            ]),
        ])
    );
}

#[test]
fn grandchild_under_taken_middle() {
    assert_eq!(
        value("users { name, o = orders.take(2) { total, i = items { sku } } }"),
        Value::List(vec![
            Value::Record(vec![
                ("name".to_string(), Value::String("Ada".to_string())),
                (
                    "o".to_string(),
                    Value::List(vec![
                        Value::Record(vec![
                            ("total".to_string(), Value::Dec(dec("10"))),
                            (
                                "i".to_string(),
                                Value::List(vec![
                                    Value::Record(vec![(
                                        "sku".to_string(),
                                        Value::String("a".to_string())
                                    )]),
                                    Value::Record(vec![(
                                        "sku".to_string(),
                                        Value::String("b".to_string())
                                    )]),
                                ])
                            ),
                        ]),
                        Value::Record(vec![
                            ("total".to_string(), Value::Dec(dec("20"))),
                            ("i".to_string(), Value::List(vec![])),
                        ]),
                    ])
                ),
            ]),
            Value::Record(vec![
                ("name".to_string(), Value::String("Bob".to_string())),
                (
                    "o".to_string(),
                    Value::List(vec![Value::Record(vec![
                        ("total".to_string(), Value::Dec(dec("30"))),
                        (
                            "i".to_string(),
                            Value::List(vec![Value::Record(vec![(
                                "sku".to_string(),
                                Value::String("c".to_string())
                            )])])
                        ),
                    ])]),
                ),
            ]),
        ])
    );
}

#[test]
fn grandchild_under_sorted_taken_middle() {
    assert_eq!(
        value("users { name, o = orders.sort(total).take(1) { total, i = items { sku } } }"),
        Value::List(vec![
            Value::Record(vec![
                ("name".to_string(), Value::String("Ada".to_string())),
                (
                    "o".to_string(),
                    Value::List(vec![Value::Record(vec![
                        ("total".to_string(), Value::Dec(dec("10"))),
                        (
                            "i".to_string(),
                            Value::List(vec![
                                Value::Record(vec![(
                                    "sku".to_string(),
                                    Value::String("a".to_string())
                                )]),
                                Value::Record(vec![(
                                    "sku".to_string(),
                                    Value::String("b".to_string())
                                )]),
                            ])
                        ),
                    ])]),
                ),
            ]),
            Value::Record(vec![
                ("name".to_string(), Value::String("Bob".to_string())),
                (
                    "o".to_string(),
                    Value::List(vec![Value::Record(vec![
                        ("total".to_string(), Value::Dec(dec("30"))),
                        (
                            "i".to_string(),
                            Value::List(vec![Value::Record(vec![(
                                "sku".to_string(),
                                Value::String("c".to_string())
                            )])])
                        ),
                    ])]),
                ),
            ]),
        ])
    );
}

fn cfg(name: &str, owner: &str) -> Value {
    Value::Record(vec![
        ("name".to_string(), Value::String(name.to_string())),
        ("owner".to_string(), Value::String(owner.to_string())),
    ])
}

#[test]
fn cross_disjunctive_pairs_and_broadcasts() {
    assert_eq!(
        value(r#"users { x = configs.filter(owner == email || name == "n1") }"#),
        Value::List(vec![
            Value::Record(vec![(
                "x".to_string(),
                Value::List(vec![cfg("a@x", "a@x"), cfg("n1", "a@x"), cfg("n1", "zzz")])
            )]),
            Value::Record(vec![(
                "x".to_string(),
                Value::List(vec![cfg("n3", "b@x"), cfg("n1", "a@x"), cfg("n1", "zzz")])
            )]),
        ])
    );
}

#[test]
fn cross_disjunctive_shared_column_pairs_once() {
    assert_eq!(
        value("users { x = configs.filter(owner == email || name == email) }"),
        Value::List(vec![
            Value::Record(vec![(
                "x".to_string(),
                Value::List(vec![cfg("n1", "a@x"), cfg("a@x", "a@x")])
            )]),
            Value::Record(vec![(
                "x".to_string(),
                Value::List(vec![cfg("n3", "b@x"), cfg("b@x", "zzz")])
            )]),
        ])
    );
}

#[test]
fn cross_disjunctive_single_prefers_keyed() {
    assert_eq!(
        value(r#"users { x = configs.filter(owner == email || name == "n1").first() }"#),
        Value::List(vec![
            Value::Record(vec![("x".to_string(), cfg("a@x", "a@x"))]),
            Value::Record(vec![("x".to_string(), cfg("n3", "b@x"))]),
        ])
    );
}

#[test]
fn cross_disjunctive_temp_parent() {
    assert_eq!(
        value(r#"users.filter(age > 30) { x = configs.filter(owner == email || name == "n1") }"#),
        Value::List(vec![Value::Record(vec![(
            "x".to_string(),
            Value::List(vec![cfg("n3", "b@x"), cfg("n1", "a@x"), cfg("n1", "zzz")])
        )])])
    );
}

#[test]
fn cross_take_skip_counts_per_parent() {
    let Value::List(rows) = value("users { x = configs.filter(owner == email).take(1) }") else {
        panic!("root list");
    };
    assert_eq!(rows.len(), 2);
    let lens: Vec<usize> = rows
        .iter()
        .map(|r| match r {
            Value::Record(fields) => match &fields[..] {
                [(_, Value::List(items))] => items.len(),
                _ => panic!("x list"),
            },
            _ => panic!("parent record"),
        })
        .collect();
    assert_eq!(lens, vec![1, 1]);
    let Value::List(rows) = value("users { x = configs.filter(owner == email).skip(1) }") else {
        panic!("root list");
    };
    let lens: Vec<usize> = rows
        .iter()
        .map(|r| match r {
            Value::Record(fields) => match &fields[..] {
                [(_, Value::List(items))] => items.len(),
                _ => panic!("x list"),
            },
            _ => panic!("parent record"),
        })
        .collect();
    assert_eq!(lens, vec![1, 0]);
}

#[test]
fn cross_sorted_take_is_deterministic() {
    assert_eq!(
        value("users { x = configs.filter(owner == email).sort(name).take(1) }"),
        Value::List(vec![
            Value::Record(vec![(
                "x".to_string(),
                Value::List(vec![cfg("a@x", "a@x")])
            )]),
            Value::Record(vec![("x".to_string(), Value::List(vec![cfg("n3", "b@x")]))]),
        ])
    );
}

#[test]
fn cross_take_under_filter_applies_before_pairing() {
    let Value::List(rows) = value("users { x = configs.take(1).filter(owner == email) }") else {
        panic!("root list");
    };
    assert_eq!(rows.len(), 2);
    let emails = ["a@x", "b@x"];
    let mut total = 0;
    for (row, email) in rows.iter().zip(emails) {
        match row {
            Value::Record(fields) => match &fields[..] {
                [(_, Value::List(items))] => {
                    total += items.len();
                    for item in items {
                        match item {
                            Value::Record(fields) => match &fields[..] {
                                [(_, Value::String(_)), (_, Value::String(owner))] => {
                                    assert_eq!(owner, email)
                                }
                                _ => panic!("config record"),
                            },
                            _ => panic!("config item"),
                        }
                    }
                }
                _ => panic!("x list"),
            },
            _ => panic!("parent record"),
        }
    }
    assert!(total <= 1);
}

#[test]
fn cross_sorted_global_take_pairs_first_alphabetically() {
    assert_eq!(
        value("users { x = configs.sort(name).take(1).filter(owner == email) }"),
        Value::List(vec![
            Value::Record(vec![(
                "x".to_string(),
                Value::List(vec![cfg("a@x", "a@x")])
            )]),
            Value::Record(vec![("x".to_string(), Value::List(vec![]))]),
        ])
    );
}

fn u_consts(owner: &str) -> Value {
    let rec = || Value::Record(vec![("n".to_string(), Value::String(owner.to_string()))]);
    Value::List(vec![rec(), rec()])
}

#[test]
fn cross_projection_outer_consts_pair_full_set() {
    assert_eq!(
        value("configs { u = users { n = owner } }"),
        Value::List(
            ["a@x", "zzz", "b@x", "zzz", "a@x", "zzz"]
                .iter()
                .map(|o| Value::Record(vec![("u".to_string(), u_consts(o))]))
                .collect()
        )
    );
}

#[test]
fn cross_projection_outer_consts_keep_siblings() {
    assert_eq!(
        value(r#"configs { name, u = users { n = owner } }"#),
        Value::List(
            [
                ("n1", "a@x"),
                ("n2", "zzz"),
                ("n3", "b@x"),
                ("b@x", "zzz"),
                ("a@x", "a@x"),
                ("n1", "zzz"),
            ]
            .iter()
            .map(|(name, o)| Value::Record(vec![
                ("name".to_string(), Value::String(name.to_string())),
                ("u".to_string(), u_consts(o)),
            ]))
            .collect()
        )
    );
}

#[test]
fn cross_inequality_pairs_complement() {
    assert_eq!(
        value("users { x = configs.filter(owner != email) }"),
        Value::List(vec![
            Value::Record(vec![(
                "x".to_string(),
                Value::List(vec![
                    cfg("n2", "zzz"),
                    cfg("n3", "b@x"),
                    cfg("b@x", "zzz"),
                    cfg("n1", "zzz"),
                ])
            )]),
            Value::Record(vec![(
                "x".to_string(),
                Value::List(vec![
                    cfg("n1", "a@x"),
                    cfg("n2", "zzz"),
                    cfg("b@x", "zzz"),
                    cfg("a@x", "a@x"),
                    cfg("n1", "zzz"),
                ])
            )]),
        ])
    );
}

#[test]
fn middle_scoped_literal_collapse_exec() {
    let one = || Value::Record(vec![("t".to_string(), Value::List(vec![Value::Int(1)]))]);
    assert_eq!(
        value("users { o = orders { t = [[1]].first() } }"),
        Value::List(vec![
            Value::Record(vec![("o".to_string(), Value::List(vec![one(), one()]))]),
            Value::Record(vec![("o".to_string(), Value::List(vec![one()]))]),
        ])
    );
}

#[test]
fn middle_scoped_storage_chain_exec() {
    let sku = |s: &str| Value::Record(vec![("sku".to_string(), Value::String(s.to_string()))]);
    assert_eq!(
        value("users { o = subordinates { t = orders.sort(total).first()?.items } }"),
        Value::List(vec![
            Value::Record(vec![(
                "o".to_string(),
                Value::List(vec![Value::Record(vec![(
                    "t".to_string(),
                    Value::List(vec![sku("c")]),
                )])])
            )]),
            Value::Record(vec![(
                "o".to_string(),
                Value::List(vec![Value::Record(vec![(
                    "t".to_string(),
                    Value::List(vec![sku("a"), sku("b")]),
                )])])
            )]),
        ])
    );
}

#[test]
fn unwrap_or_rows_falls_back() {
    let one = || Value::List(vec![Value::Int(1)]);
    assert_eq!(
        value("users { o = [[1]].first().unwrap_or([2]) }"),
        Value::List(vec![
            Value::Record(vec![("o".to_string(), one())]),
            Value::Record(vec![("o".to_string(), one())]),
        ])
    );
    assert_eq!(
        value("users { o = [[], [1]].first().unwrap_or([9]) }"),
        Value::List(vec![
            Value::Record(vec![("o".to_string(), Value::List(vec![]))]),
            Value::Record(vec![("o".to_string(), Value::List(vec![]))]),
        ])
    );
    assert_eq!(
        value("users { o = [[1]].nth(5).unwrap_or([9]) }"),
        Value::List(vec![
            Value::Record(vec![("o".to_string(), Value::List(vec![Value::Int(9)]))]),
            Value::Record(vec![("o".to_string(), Value::List(vec![Value::Int(9)]))]),
        ])
    );
}

#[test]
fn single_base_nav_chains_exec() {
    let Value::List(rows) = value("users { t = manager.orders.first()?.total }") else {
        panic!("root list");
    };
    assert_eq!(rows.len(), 2);
    let totals: Vec<Value> = rows
        .iter()
        .map(|r| match r {
            Value::Record(fields) => match &fields[..] {
                [(_, total)] => total.clone(),
                _ => panic!("t field"),
            },
            _ => panic!("parent record"),
        })
        .collect();
    assert_eq!(totals[0], Value::Dec(dec("30")));
    assert!(
        totals[1] == Value::Dec(dec("10")) || totals[1] == Value::Dec(dec("20")),
        "bag-pick, got {:?}",
        totals[1]
    );
}

#[test]
fn agg_scoped_nav_element_rooted_exec() {
    assert_eq!(
        value("users { s = orders.sum_over(user.age) }"),
        Value::List(vec![
            Value::Record(vec![("s".to_string(), Value::Int(60))]),
            Value::Record(vec![("s".to_string(), Value::Int(40))]),
        ])
    );
}

#[test]
fn m2m_pairs_through_junction() {
    let role =
        |name: &str| Value::Record(vec![("name".to_string(), Value::String(name.to_string()))]);
    assert_eq!(
        value("users { r = roles }"),
        Value::List(vec![
            Value::Record(vec![(
                "r".to_string(),
                Value::List(vec![role("admin"), role("dev")])
            )]),
            Value::Record(vec![("r".to_string(), Value::List(vec![role("dev")]))]),
        ])
    );
}

#[test]
fn m2m_reverse_pairs() {
    assert_eq!(
        value(r#"roles { u = users { n = name } }"#),
        Value::List(vec![
            Value::Record(vec![(
                "u".to_string(),
                Value::List(vec![Value::Record(vec![(
                    "n".to_string(),
                    Value::String("Ada".to_string()),
                )])])
            )]),
            Value::Record(vec![(
                "u".to_string(),
                Value::List(vec![
                    Value::Record(vec![("n".to_string(), Value::String("Ada".to_string()))]),
                    Value::Record(vec![("n".to_string(), Value::String("Bob".to_string()))]),
                ])
            )]),
        ])
    );
}

#[test]
fn m2m_nested_pairs_per_middle() {
    let role =
        |name: &str| Value::Record(vec![("name".to_string(), Value::String(name.to_string()))]);
    assert_eq!(
        value("users { o = subordinates { r = roles } }"),
        Value::List(vec![
            Value::Record(vec![(
                "o".to_string(),
                Value::List(vec![Value::Record(vec![(
                    "r".to_string(),
                    Value::List(vec![role("dev")]),
                )])])
            )]),
            Value::Record(vec![(
                "o".to_string(),
                Value::List(vec![Value::Record(vec![(
                    "r".to_string(),
                    Value::List(vec![role("admin"), role("dev")]),
                )])])
            )]),
        ])
    );
}

#[test]
fn cross_nav_unwrap_or_coalesce_exec() {
    assert_eq!(
        value(r#"users { x = configs.filter(owner == mentor?.email.unwrap_or("zzz")) }"#),
        Value::List(vec![
            Value::Record(vec![(
                "x".to_string(),
                Value::List(vec![cfg("n2", "zzz"), cfg("b@x", "zzz"), cfg("n1", "zzz"),])
            )]),
            Value::Record(vec![(
                "x".to_string(),
                Value::List(vec![cfg("n1", "a@x"), cfg("a@x", "a@x"),])
            )]),
        ])
    );
}

#[test]
fn chained_over_collapsed_exec() {
    assert_eq!(
        value("users.first()?.manager?.name"),
        Value::String("Bob".to_string())
    );
}

#[test]
fn unwrap_nav_field_aborts_on_missing() {
    let mut rt = fixture();
    let compiled = compile("users { m = mentor.unwrap().name }");
    let err = rt.execute_value(&compiled).unwrap_err();
    assert!(
        matches!(err, RuntimeError::Aborted(_)),
        "expected abort, got {err:?}"
    );
}

#[test]
fn unwrap_nav_field_present_values() {
    assert_eq!(
        value("users.filter(age > 30) { m = mentor.unwrap().name }"),
        Value::List(vec![Value::Record(vec![(
            "m".to_string(),
            Value::String("Ada".to_string())
        )])])
    );
}

#[test]
fn unwrap_nav_field_filter_aborts() {
    let mut rt = fixture();
    let compiled = compile("users.filter(mentor.unwrap().age > 20)");
    let err = rt.execute_value(&compiled).unwrap_err();
    assert!(
        matches!(err, RuntimeError::Aborted(_)),
        "expected abort, got {err:?}"
    );
}

#[test]
fn cross_proj_take_temp_pairs() {
    let Value::List(rows) = value("users { e = email } { x = configs.filter(owner == e).take(1) }")
    else {
        panic!("root list");
    };
    assert_eq!(rows.len(), 2);
    for r in &rows {
        match r {
            Value::Record(fields) => match &fields[..] {
                [(_, Value::List(items))] => assert_eq!(items.len(), 1),
                _ => panic!("x list"),
            },
            _ => panic!("parent record"),
        }
    }
    assert_eq!(
        value("users { n = name } { x = configs.filter(owner == n).take(1) }"),
        Value::List(vec![
            Value::Record(vec![("x".to_string(), Value::List(vec![]))]),
            Value::Record(vec![("x".to_string(), Value::List(vec![]))]),
        ])
    );
}

#[test]
fn cross_proj_corr_bare_temp_pairs() {
    assert_eq!(
        value("users { e = email } { x = configs.filter(owner == e) }"),
        Value::List(vec![
            Value::Record(vec![(
                "x".to_string(),
                Value::List(vec![cfg("n1", "a@x"), cfg("a@x", "a@x")])
            )]),
            Value::Record(vec![("x".to_string(), Value::List(vec![cfg("n3", "b@x")]))]),
        ])
    );
}

#[test]
fn cross_per_row_take_all() {
    let all = vec![
        cfg("n1", "a@x"),
        cfg("n2", "zzz"),
        cfg("n3", "b@x"),
        cfg("b@x", "zzz"),
        cfg("a@x", "a@x"),
        cfg("n1", "zzz"),
    ];
    assert_eq!(
        value("users { x = configs.take(age) }"),
        Value::List(vec![
            Value::Record(vec![("x".to_string(), Value::List(all.clone()))]),
            Value::Record(vec![("x".to_string(), Value::List(all))]),
        ])
    );
}

#[test]
fn cross_per_row_skip_all() {
    assert_eq!(
        value("users { x = configs.skip(age) }"),
        Value::List(vec![
            Value::Record(vec![("x".to_string(), Value::List(vec![]))]),
            Value::Record(vec![("x".to_string(), Value::List(vec![]))]),
        ])
    );
}

#[test]
fn cross_per_row_narrow_parent() {
    let all = vec![
        cfg("n1", "a@x"),
        cfg("n2", "zzz"),
        cfg("n3", "b@x"),
        cfg("b@x", "zzz"),
        cfg("a@x", "a@x"),
        cfg("n1", "zzz"),
    ];
    assert_eq!(
        value("users { name, x = configs.take(age) }"),
        Value::List(vec![
            Value::Record(vec![
                ("name".to_string(), Value::String("Ada".to_string())),
                ("x".to_string(), Value::List(all.clone())),
            ]),
            Value::Record(vec![
                ("name".to_string(), Value::String("Bob".to_string())),
                ("x".to_string(), Value::List(all)),
            ]),
        ])
    );
}

#[test]
fn cross_per_row_projected_bound() {
    let all = vec![
        cfg("n1", "a@x"),
        cfg("n2", "zzz"),
        cfg("n3", "b@x"),
        cfg("b@x", "zzz"),
        cfg("a@x", "a@x"),
        cfg("n1", "zzz"),
    ];
    assert_eq!(
        value("users { a = age } { x = configs.take(a) }"),
        Value::List(vec![
            Value::Record(vec![("x".to_string(), Value::List(all.clone()))]),
            Value::Record(vec![("x".to_string(), Value::List(all))]),
        ])
    );
}

#[test]
fn cross_per_row_local_filter() {
    let both = vec![cfg("n1", "a@x"), cfg("n1", "zzz")];
    assert_eq!(
        value(r#"users { x = configs.filter(name == "n1").take(age) }"#),
        Value::List(vec![
            Value::Record(vec![("x".to_string(), Value::List(both.clone()))]),
            Value::Record(vec![("x".to_string(), Value::List(both))]),
        ])
    );
}

#[test]
fn cross_per_row_empty_parents() {
    assert_eq!(
        value("users.filter(age > 100) { x = configs.take(age) }"),
        Value::List(vec![])
    );
}

#[test]
fn cross_nav_outer_pairs_by_carried_endpoint() {
    assert_eq!(
        value("users { x = configs.filter(owner == manager.email) }"),
        Value::List(vec![
            Value::Record(vec![("x".to_string(), Value::List(vec![cfg("n3", "b@x")]))]),
            Value::Record(vec![(
                "x".to_string(),
                Value::List(vec![cfg("n1", "a@x"), cfg("a@x", "a@x")])
            )]),
        ])
    );
}

#[test]
fn cross_nav_outer_name_matches_nothing() {
    assert_eq!(
        value("users { x = configs.filter(owner == manager.name) }"),
        Value::List(vec![
            Value::Record(vec![("x".to_string(), Value::List(vec![]))]),
            Value::Record(vec![("x".to_string(), Value::List(vec![]))]),
        ])
    );
}

#[test]
fn cross_nav_outer_with_broadcast() {
    assert_eq!(
        value(r#"users { x = configs.filter(owner == manager.email || name == "n1") }"#),
        Value::List(vec![
            Value::Record(vec![(
                "x".to_string(),
                Value::List(vec![cfg("n3", "b@x"), cfg("n1", "a@x"), cfg("n1", "zzz")])
            )]),
            Value::Record(vec![(
                "x".to_string(),
                Value::List(vec![cfg("a@x", "a@x"), cfg("n1", "a@x"), cfg("n1", "zzz")])
            )]),
        ])
    );
}

#[test]
fn cross_sort_mixed_orders_per_parent() {
    assert_eq!(
        value("users { x = configs.sort(name == email, name) }"),
        Value::List(vec![
            Value::Record(vec![(
                "x".to_string(),
                Value::List(vec![
                    cfg("b@x", "zzz"),
                    cfg("n1", "a@x"),
                    cfg("n1", "zzz"),
                    cfg("n2", "zzz"),
                    cfg("n3", "b@x"),
                    cfg("a@x", "a@x"),
                ])
            )]),
            Value::Record(vec![(
                "x".to_string(),
                Value::List(vec![
                    cfg("a@x", "a@x"),
                    cfg("n1", "a@x"),
                    cfg("n1", "zzz"),
                    cfg("n2", "zzz"),
                    cfg("n3", "b@x"),
                    cfg("b@x", "zzz"),
                ])
            )]),
        ])
    );
}

#[test]
fn cross_sort_mixed_desc() {
    assert_eq!(
        value("users { x = configs.sort_desc(name == email, name) }"),
        Value::List(vec![
            Value::Record(vec![(
                "x".to_string(),
                Value::List(vec![
                    cfg("a@x", "a@x"),
                    cfg("n3", "b@x"),
                    cfg("n2", "zzz"),
                    cfg("n1", "a@x"),
                    cfg("n1", "zzz"),
                    cfg("b@x", "zzz"),
                ])
            )]),
            Value::Record(vec![(
                "x".to_string(),
                Value::List(vec![
                    cfg("b@x", "zzz"),
                    cfg("n3", "b@x"),
                    cfg("n2", "zzz"),
                    cfg("n1", "a@x"),
                    cfg("n1", "zzz"),
                    cfg("a@x", "a@x"),
                ])
            )]),
        ])
    );
}

#[test]
fn cross_sort_mixed_over_projection() {
    let m = |name: &str| Value::Record(vec![("m".to_string(), Value::String(name.to_string()))]);
    assert_eq!(
        value(r#"users { x = configs { m = name }.sort(m == email, m) }"#),
        Value::List(vec![
            Value::Record(vec![(
                "x".to_string(),
                Value::List(vec![m("b@x"), m("n1"), m("n1"), m("n2"), m("n3"), m("a@x"),])
            )]),
            Value::Record(vec![(
                "x".to_string(),
                Value::List(vec![m("a@x"), m("n1"), m("n1"), m("n2"), m("n3"), m("b@x"),])
            )]),
        ])
    );
}

#[test]
fn cross_root_agg_in_child_tail() {
    let s = |n: i64| Value::Record(vec![("s".to_string(), Value::Int(n))]);
    assert_eq!(
        value("users { o = orders { s = configs.len() } }"),
        Value::List(vec![
            Value::Record(vec![("o".to_string(), Value::List(vec![s(6), s(6)]))]),
            Value::Record(vec![("o".to_string(), Value::List(vec![s(6)]))]),
        ])
    );
}

#[test]
fn cross_nested_collection_agg_in_child_tail() {
    let s = |n: i64| Value::Record(vec![("s".to_string(), Value::Int(n))]);
    assert_eq!(
        value("users { o = orders { s = subordinates.len() } }"),
        Value::List(vec![
            Value::Record(vec![("o".to_string(), Value::List(vec![s(1), s(1)]))]),
            Value::Record(vec![("o".to_string(), Value::List(vec![s(1)]))]),
        ])
    );
}

#[test]
fn cross_sort_pure_outer_broadcasts_order() {
    let all = vec![
        cfg("n1", "a@x"),
        cfg("n2", "zzz"),
        cfg("n3", "b@x"),
        cfg("b@x", "zzz"),
        cfg("a@x", "a@x"),
        cfg("n1", "zzz"),
    ];
    assert_eq!(
        value("users { x = configs.sort(email) }"),
        Value::List(vec![
            Value::Record(vec![("x".to_string(), Value::List(all.clone()))]),
            Value::Record(vec![("x".to_string(), Value::List(all))]),
        ])
    );
}

#[test]
fn cross_range_pairs_direction_correctly() {
    assert_eq!(
        value("users { x = configs.filter(owner > email) }"),
        Value::List(vec![
            Value::Record(vec![(
                "x".to_string(),
                Value::List(vec![
                    cfg("n2", "zzz"),
                    cfg("n3", "b@x"),
                    cfg("b@x", "zzz"),
                    cfg("n1", "zzz"),
                ])
            )]),
            Value::Record(vec![("x".to_string(), Value::List(vec![]))]),
        ])
    );
    assert_eq!(
        value("users { x = configs.filter(email > owner) }"),
        Value::List(vec![
            Value::Record(vec![("x".to_string(), Value::List(vec![]))]),
            Value::Record(vec![(
                "x".to_string(),
                Value::List(vec![cfg("n1", "a@x"), cfg("a@x", "a@x")])
            )]),
        ])
    );
}


fn dummy_stmts(n: usize) -> Vec<PlannedStatement> {
    (0..n)
        .map(|_| PlannedStatement {
            sql: String::new(),
            params: vec![],
            shape: StatementShape {
                columns: vec![],
                rows: RowCount::Many,
            },
            kind: StatementKind::Query,
        })
        .collect()
}

fn synth(result: ResultShape, outputs: Vec<StatementOutput>) -> Result<Value, RuntimeError> {
    let n = outputs.len();
    assemble(
        &CompiledQuery {
            statements: dummy_stmts(n),
            result,
            literals: HashMap::new(),
        },
        &outputs,
    )
}

#[test]
fn single_unkeyed_first_or_absent() {
    let shape = ResultShape::Single {
        element: Box::new(int_col("v")),
        child: unkeyed(0, Cardinality::One),
    };
    let outputs = vec![StatementOutput {
        rows: vec![row(&[("v", Value::Int(1))]), row(&[("v", Value::Int(2))])],
    }];
    assert_eq!(synth(shape.clone(), outputs).unwrap(), Value::Int(1));
    let empty = vec![StatementOutput { rows: vec![] }];
    assert_eq!(synth(shape, empty).unwrap(), Value::Null);
}

#[test]
fn single_keyed_hit_miss_and_null() {
    let shape = ResultShape::Single {
        element: Box::new(int_col("v")),
        child: keyed(1, 0, "id", "fk"),
    };
    let parents = StatementOutput {
        rows: vec![
            row(&[("id", Value::Int(1))]),
            row(&[("id", Value::Int(2))]),
            row(&[("id", Value::Null)]),
        ],
    };
    let children = StatementOutput {
        rows: vec![row(&[("v", Value::Int(10)), ("fk", Value::Int(1))])],
    };
    assert_eq!(
        synth(shape, vec![parents, children]).unwrap(),
        Value::Int(10)
    );
    let parents = StatementOutput {
        rows: vec![row(&[("id", Value::Int(2))]), row(&[("id", Value::Null)])],
    };
    let children = StatementOutput {
        rows: vec![row(&[("v", Value::Int(10)), ("fk", Value::Int(1))])],
    };
    let shape = ResultShape::Single {
        element: Box::new(int_col("v")),
        child: keyed(1, 0, "id", "fk"),
    };
    assert_eq!(synth(shape, vec![parents, children]).unwrap(), Value::Null);
}

#[test]
fn single_keyed_falls_back_to_broadcast() {
    let shape = ResultShape::Single {
        element: Box::new(int_col("v")),
        child: keyed(1, 0, "id", "fk"),
    };
    let parents = StatementOutput {
        rows: vec![
            row(&[("id", Value::Null)]),
            row(&[("id", Value::Int(1))]),
            row(&[("id", Value::Int(2))]),
        ],
    };
    let children = StatementOutput {
        rows: vec![
            row(&[
                ("v", Value::Int(10)),
                ("fk", Value::Int(2)),
                ("$$ptag", Value::Int(0)),
            ]),
            row(&[
                ("v", Value::Int(99)),
                ("fk", Value::Null),
                ("$$ptag", Value::Int(1)),
            ]),
        ],
    };
    assert_eq!(
        synth(shape, vec![parents, children]).unwrap(),
        Value::Int(99)
    );
}

#[test]
fn list_null_key_still_broadcasts() {
    let shape = ResultShape::List {
        element: Box::new(int_col("v")),
        child: keyed(1, 0, "id", "fk"),
    };
    let parents = StatementOutput {
        rows: vec![row(&[("id", Value::Null)]), row(&[("id", Value::Int(2))])],
    };
    let children = StatementOutput {
        rows: vec![
            row(&[
                ("v", Value::Int(10)),
                ("fk", Value::Int(2)),
                ("$$ptag", Value::Int(0)),
            ]),
            row(&[
                ("v", Value::Int(99)),
                ("fk", Value::Null),
                ("$$ptag", Value::Int(1)),
            ]),
        ],
    };
    assert_eq!(
        synth(shape, vec![parents, children]).unwrap(),
        Value::List(vec![Value::Int(99), Value::Int(10), Value::Int(99)])
    );
}

#[test]
fn single_nested_uses_parent_row() {
    let shape = ResultShape::Record(vec![(
        "f".to_string(),
        ResultShape::Single {
            element: Box::new(int_col("v")),
            child: keyed(1, 0, "id", "fk"),
        },
    )]);
    let root = StatementOutput {
        rows: vec![row(&[("id", Value::Int(2))])],
    };
    let children = StatementOutput {
        rows: vec![
            row(&[("v", Value::Int(10)), ("fk", Value::Int(1))]),
            row(&[("v", Value::Int(20)), ("fk", Value::Int(2))]),
        ],
    };
    assert_eq!(
        synth(shape, vec![root, children]).unwrap(),
        Value::Record(vec![("f".to_string(), Value::Int(20))])
    );
}

#[test]
fn list_keyed_cardinality_one_truncates() {
    let shape = ResultShape::List {
        element: Box::new(int_col("v")),
        child: ChildRef {
            stmt: 1,
            parent: Some(ParentRef {
                stmt: 0,
                parent_key: "id".to_string(),
                child_key: "fk".to_string(),
                negated: false,
            }),
            cardinality: Cardinality::One,
            tag: None,
        },
    };
    let parents = StatementOutput {
        rows: vec![row(&[("id", Value::Int(1))])],
    };
    let children = StatementOutput {
        rows: vec![
            row(&[("v", Value::Int(10)), ("fk", Value::Int(1))]),
            row(&[("v", Value::Int(20)), ("fk", Value::Int(1))]),
        ],
    };
    assert_eq!(
        synth(shape, vec![parents, children]).unwrap(),
        Value::List(vec![Value::Int(10)])
    );
}

#[test]
fn list_keyed_null_key_matches_nothing() {
    let shape = ResultShape::List {
        element: Box::new(int_col("v")),
        child: keyed(1, 0, "id", "fk"),
    };
    let parents = StatementOutput {
        rows: vec![row(&[("id", Value::Null)])],
    };
    let children = StatementOutput {
        rows: vec![row(&[("v", Value::Int(10)), ("fk", Value::Null)])],
    };
    assert_eq!(
        synth(shape, vec![parents, children]).unwrap(),
        Value::List(vec![])
    );
}

#[test]
fn tags_fail_closed() {
    let tagged = ChildRef {
        stmt: 0,
        parent: None,
        cardinality: Cardinality::Many,
        tag: Some("t0".to_string()),
    };
    let outputs = vec![StatementOutput {
        rows: vec![row(&[("v", Value::Int(1))])],
    }];
    let list = ResultShape::List {
        element: Box::new(int_col("v")),
        child: tagged.clone(),
    };
    assert!(matches!(
        synth(list, outputs.clone()).unwrap_err(),
        RuntimeError::Contract(_)
    ));
    let single = ResultShape::Single {
        element: Box::new(int_col("v")),
        child: tagged,
    };
    assert!(matches!(
        synth(single, outputs).unwrap_err(),
        RuntimeError::Contract(_)
    ));
}

#[test]
fn key_failures_are_contract_errors() {
    let shape = ResultShape::List {
        element: Box::new(int_col("v")),
        child: keyed(1, 0, "id", "fk"),
    };
    let outputs = vec![
        StatementOutput {
            rows: vec![row(&[("id", Value::Int(1))])],
        },
        StatementOutput {
            rows: vec![row(&[("v", Value::Int(10))])],
        },
    ];
    assert!(matches!(
        synth(shape, outputs).unwrap_err(),
        RuntimeError::Contract(_)
    ));
    let shape = ResultShape::List {
        element: Box::new(int_col("v")),
        child: keyed(1, 0, "id", "fk"),
    };
    let outputs = vec![
        StatementOutput {
            rows: vec![row(&[("id", Value::List(vec![]))])],
        },
        StatementOutput {
            rows: vec![row(&[("v", Value::Int(10)), ("fk", Value::Int(1))])],
        },
    ];
    assert!(matches!(
        synth(shape, outputs).unwrap_err(),
        RuntimeError::Contract(_)
    ));
    let outputs = vec![StatementOutput {
        rows: vec![row(&[("v", Value::Int(1))]), row(&[("v", Value::Int(2))])],
    }];
    assert!(matches!(
        synth(int_col("v"), outputs).unwrap_err(),
        RuntimeError::Contract(_)
    ));
}
