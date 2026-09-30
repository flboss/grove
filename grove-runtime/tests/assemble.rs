
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
fn empty_root_list() {
    assert_eq!(value("users[age > 100] { name }"), Value::List(vec![]));
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
